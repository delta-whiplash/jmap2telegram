//! Health endpoint, readiness and Prometheus metrics for Kubernetes.
//!
//! The bot has no server of its own (it's a Telegram long-poller plus JMAP
//! EventSource watchers), so this module provides the smallest possible
//! surface kubelet and vmagent can talk to: a hand-rolled HTTP/1.1
//! responder on a dedicated listener - no HTTP framework dependency, no
//! shared state that can deadlock the request path.
//!
//! Contract, deliberately split by what each signal is *for*:
//!
//! - **`/healthz` (liveness) stays conservative on purpose.** It answers
//!   200 as long as the tokio runtime is scheduling the accept loop. A
//!   false-positive liveness kill is the worst outcome for a single-replica
//!   stateful bot - it interrupts the Telegram poll loop and drops undo
//!   state - so liveness deliberately does NOT encode watcher health. A
//!   wedged runtime (deadlocked scheduler, exhausted memory) stops the
//!   accept loop and the kubelet timeout restarts the pod.
//!
//! - **`/readyz` (readiness) reflects the watchers.** 200 when every
//!   registered watcher currently holds an open EventSource connection (or
//!   when none are registered yet - boot, or a bot with no `/login` at
//!   all). A watcher stuck in reconnect backoff makes the pod NotReady.
//!   There is no Service in front of this bot, so NotReady doesn't shed
//!   traffic; what it does is surface a *silently broken* bot (revoked
//!   token, unreachable JMAP server) in `kubectl` and - via the pod Ready
//!   condition - in ArgoCD application health and the cluster's
//!   ArgoCDAppDegraded alert, without restarting anything that a backoff
//!   loop can heal on its own.
//!
//!   Registration is what makes that honest, so it happens *before* the
//!   first connect attempt everywhere: `watcher::spawn` registers its key
//!   before its task starts, and boot resume (see `accounts.rs`) registers
//!   every persisted account up-front, then retries the connect with the
//!   watcher's own backoff. An account whose server never comes back is
//!   therefore a *permanent* registered-but-disconnected entry - /readyz
//!   503 naming it - and never a green pod that quietly watches nothing.
//!   The flip side is the pairing obligation: a registration must be
//!   dropped on every teardown path (`Health::deregister` /
//!   `deregister_chat`), including `handle.abort()`, since an aborted task
//!   never gets to run its own cleanup.
//!
//! - **`/metrics` exposes Prometheus text format** so vmagent can scrape
//!   the same registry: `jmap2telegram_watchers_registered` /
//!   `_connected` gauges and per-watcher
//!   `jmap2telegram_last_activity_age_seconds`. `last_activity` advances
//!   on a successful JMAP sync (EventSource wake-up or the 5-minute
//!   fallback poll), so an age above `FALLBACK_POLL` + margin means that
//!   watcher is neither receiving events nor completing fallback syncs -
//!   alert material, not restart material.
//!
//!   Alongside those come the [`Metrics`] the request handlers maintain:
//!   `jmap2telegram_build_info`, `_start_time_seconds`, `_accounts`,
//!   `_pending_undos`, `process_resident_memory_bytes`, and the
//!   `_notifications_delivered_total` / `_notification_send_errors_total`
//!   / `_triage_actions_total` / `_mark_read_total` / `_undo_total` /
//!   `_watcher_reconnects_total` / `_watcher_syncs_total` counters. No
//!   label ever carries a chat id or any user-supplied value, so a
//!   scrape's cardinality is bounded by construction.
//!
//! Two hardening rules apply to the wire path itself:
//!
//! - **Request reads are deadline-bounded** (see [`REQUEST_DEADLINE`]). A
//!   connection that opens and then never sends a byte - a port scanner, a
//!   wedged scraper - is closed when the deadline expires, instead of
//!   pinning the accept-spawned task and its socket forever.
//!
//! - **Watcher keys are escaped before they reach a probe response.** Part
//!   of a key is the JMAP account id, chosen by the *server* (see
//!   `watcher::shared_health_key`), so it is not trusted to be
//!   format-safe: [`escape_prometheus_label`] keeps a key containing `\`,
//!   `"` or a newline from breaking the scrape or injecting fake metric
//!   lines.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One watched JMAP account's live state, as tracked by its watcher task.
#[derive(Clone, Debug)]
pub struct WatcherStatus {
    /// Whether the EventSource connection is currently open.
    pub connected: bool,
    /// When the last successful JMAP sync completed (event-driven or
    /// fallback poll). `None` until the first sync after watcher start.
    pub last_activity: Option<Instant>,
}

/// Registry of live watchers, updated by the watcher tasks and read by the
/// probe/metrics endpoints. One `Mutex` over a small map: contention is
/// nil (updates happen per event / per 5-minute poll, reads per probe).
#[derive(Default)]
pub struct Health {
    watchers: Mutex<HashMap<String, WatcherStatus>>,
}

impl Health {
    fn update<F: FnOnce(&mut WatcherStatus)>(&self, key: &str, f: F) {
        let mut map = self.watchers.lock().unwrap();
        match map.get_mut(key) {
            Some(status) => f(status),
            None => {
                // A deregister racing an in-flight update (watcher exiting
                // while its final connect attempt lands) must not resurrect
                // the entry.
                tracing::trace!(watcher = key, "health update for unregistered watcher");
            }
        }
    }

    /// Registers a watcher before its task starts. Callers must pair this
    /// with [`deregister`] on clean exit so the registry never leaks
    /// entries for accounts that stopped being watched.
    pub fn register(&self, key: &str) {
        let mut map = self.watchers.lock().unwrap();
        map.insert(
            key.to_string(),
            WatcherStatus {
                connected: false,
                last_activity: None,
            },
        );
    }

    pub fn deregister(&self, key: &str) {
        self.watchers.lock().unwrap().remove(key);
    }

    /// Drops every registry entry belonging to `chat_id` - primary,
    /// shared and extra slots alike - in one sweep. The full-chat teardown
    /// in `state.rs` (`forget`, on /logout and /login) needs this because
    /// it must also clear entries for targets that have no live watcher
    /// handle to iterate over, such as a boot-resume still waiting out
    /// its reconnect backoff with an entry registered *before* its first
    /// connect attempt.
    ///
    /// Prefix matching on `"{chat_id}/"` is exact: the trailing separator
    /// means chat 4's keys (`"4/primary"`, `"4/shared-…"`, …) can never
    /// match a scan for chat 42, and vice versa.
    pub fn deregister_chat(&self, chat_id: i64) {
        let prefix = format!("{chat_id}/");
        self.watchers
            .lock()
            .unwrap()
            .retain(|key, _| !key.starts_with(&prefix));
    }

    /// Records whether this watcher's EventSource connection is open.
    pub fn set_connected(&self, key: &str, connected: bool) {
        self.update(key, |s| s.connected = connected);
    }

    /// Advances the activity clock after a successful JMAP sync.
    pub fn touch(&self, key: &str) {
        self.update(key, |s| s.last_activity = Some(Instant::now()));
    }

    /// Point-in-time copy of the whole registry, as /metrics renders it.
    /// Crate-visible so the teardown and boot-resume tests in other
    /// modules can assert on the same view the endpoints serve.
    pub(crate) fn snapshot(&self) -> HashMap<String, WatcherStatus> {
        self.watchers.lock().unwrap().clone()
    }

    /// Readiness: every registered watcher connected, or nothing to watch.
    /// Returns the failing keys for the 503 body. Crate-visible for the
    /// same cross-module test reasons as [`snapshot`](Health::snapshot).
    pub(crate) fn readiness(&self) -> Result<(), Vec<String>> {
        let disconnected: Vec<String> = self
            .snapshot()
            .iter()
            .filter(|(_, s)| !s.connected)
            .map(|(k, _)| k.clone())
            .collect();
        if disconnected.is_empty() {
            Ok(())
        } else {
            Err(disconnected)
        }
    }
}

/// Application-level counters and gauges, rendered alongside the watcher
/// registry's metrics. Unlike [`Health`] - a live view the watcher tasks
/// own - this is a plain registry the request handlers bump as things
/// happen, so a scrape can answer "how much mail are we delivering, and
/// what is failing" without anyone log-scraping.
///
/// Cardinality is bounded by construction: no label ever carries a chat id
/// or any other user-supplied value, so a scrape can't grow without bound
/// no matter how many chats the deployment serves.
pub struct Metrics {
    /// Process start as Unix epoch seconds (`jmap2telegram_start_time_seconds`).
    start_time_seconds: u64,
    /// Counter families keyed by metric name, each holding its samples
    /// keyed by the rendered label set.
    counters: Mutex<BTreeMap<&'static str, Family>>,
    /// `jmap2telegram_accounts`: chats currently holding a JMAP account.
    accounts: AtomicU64,
    /// `jmap2telegram_pending_undos`: undo slots currently open.
    pending_undos: AtomicU64,
}

/// One metric family: its HELP text plus the samples seen so far. Named
/// families exist so the HELP/TYPE preamble is emitted once even when a
/// counter only ever gets bumped with one label combination.
struct Family {
    help: &'static str,
    samples: BTreeMap<String, u64>,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            start_time_seconds: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            counters: Mutex::new(BTreeMap::new()),
            accounts: AtomicU64::new(0),
            pending_undos: AtomicU64::new(0),
        }
    }

    /// Increments a labeled counter sample, creating its family on first use.
    fn inc(&self, name: &'static str, help: &'static str, labels: &[(&str, &str)]) {
        let sample = render_labels(labels);
        let mut counters = self.counters.lock().unwrap();
        let family = counters.entry(name).or_insert_with(|| Family {
            help,
            samples: BTreeMap::new(),
        });
        *family.samples.entry(sample).or_insert(0) += 1;
    }

    /// A notification was successfully delivered, by account kind.
    pub fn notification_delivered(&self, kind: &str) {
        self.inc(
            "jmap2telegram_notifications_delivered_total",
            "Notifications successfully delivered to Telegram.",
            &[("kind", kind)],
        );
    }

    /// A notification failed to send (Telegram error, flood limit, ...).
    pub fn notification_send_error(&self, kind: &str) {
        self.inc(
            "jmap2telegram_notification_send_errors_total",
            "Notifications that failed to send to Telegram.",
            &[("kind", kind)],
        );
    }

    /// A new message that was not worth a notification, by account kind
    /// and by the reason it was skipped (a draft, a copy of a sent
    /// message, a message the account wrote itself, a message already
    /// read). Both labels are bounded: the kind is one of three
    /// constants, the reason one of four.
    pub fn notification_skipped(&self, kind: &str, reason: &str) {
        self.inc(
            "jmap2telegram_notifications_skipped_total",
            "New messages that were not notified about, by the reason they were skipped.",
            &[("kind", kind), ("reason", reason)],
        );
    }

    /// A triage action (archive/spam/delete) finished, by action and outcome.
    pub fn triage_action(&self, action: &str, outcome: &str) {
        self.inc(
            "jmap2telegram_triage_actions_total",
            "Triage actions taken from notification buttons.",
            &[("action", action), ("outcome", outcome)],
        );
    }

    /// A mark-as-read attempt finished.
    pub fn mark_read(&self, outcome: &str) {
        self.inc(
            "jmap2telegram_mark_read_total",
            "Mark-as-read attempts from notification buttons.",
            &[("outcome", outcome)],
        );
    }

    /// An undo attempt finished (`restored`, `expired` or `error`).
    pub fn undo(&self, outcome: &str) {
        self.inc(
            "jmap2telegram_undo_total",
            "Undo attempts on triage actions, by outcome.",
            &[("outcome", outcome)],
        );
    }

    /// A watcher had to reconnect (its EventSource dropped or never opened).
    pub fn watcher_reconnect(&self) {
        self.inc(
            "jmap2telegram_watcher_reconnects_total",
            "Watcher EventSource reconnection attempts.",
            &[],
        );
    }

    /// A watcher completed a JMAP sync (event-driven or fallback poll).
    pub fn watcher_sync(&self) {
        self.inc(
            "jmap2telegram_watcher_syncs_total",
            "Successful watcher JMAP syncs.",
            &[],
        );
    }

    pub fn set_accounts(&self, n: u64) {
        self.accounts.store(n, Ordering::Relaxed);
    }

    pub fn set_pending_undos(&self, n: u64) {
        self.pending_undos.store(n, Ordering::Relaxed);
    }

    /// Renders every counter family plus the atomic gauges.
    fn render(&self, out: &mut String) {
        // Constant 1, version in the label: the conventional build-info
        // shape, so a dashboard can join on it.
        out.push_str(
            "# HELP jmap2telegram_build_info Build information as a constant 1, labeled with the crate version.\n\
             # TYPE jmap2telegram_build_info gauge\n",
        );
        out.push_str(&format!(
            "jmap2telegram_build_info{{version=\"{}\"}} 1\n",
            escape_prometheus_label(env!("CARGO_PKG_VERSION"))
        ));

        let uptime_gauge = |out: &mut String, name: &str, help: &str, value: u64| {
            push_metric(out, name, help, "gauge", &value.to_string());
        };
        uptime_gauge(
            out,
            "jmap2telegram_start_time_seconds",
            "Start time of the process since unix epoch in seconds.",
            self.start_time_seconds,
        );
        uptime_gauge(
            out,
            "jmap2telegram_accounts",
            "Number of chats currently holding a JMAP account.",
            self.accounts.load(Ordering::Relaxed),
        );
        uptime_gauge(
            out,
            "jmap2telegram_pending_undos",
            "Undo slots currently open (triage actions still reversible).",
            self.pending_undos.load(Ordering::Relaxed),
        );
        if let Some(rss) = process_resident_memory_bytes() {
            push_metric(
                out,
                "process_resident_memory_bytes",
                "Resident memory size in bytes.",
                "gauge",
                &rss.to_string(),
            );
        }

        let counters = self.counters.lock().unwrap();
        for (name, family) in counters.iter() {
            push_metric_header(out, name, family.help, "counter");
            for (labels, value) in &family.samples {
                if labels.is_empty() {
                    out.push_str(&format!("{name} {value}\n"));
                } else {
                    out.push_str(&format!("{name}{{{labels}}} {value}\n"));
                }
            }
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Renders a label set for the text format, escaping each value with the
/// same rules as [`escape_prometheus_label`].
fn render_labels(labels: &[(&str, &str)]) -> String {
    labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_prometheus_label(v)))
        .collect::<Vec<_>>()
        .join(",")
}

/// Appends one `# HELP` / `# TYPE` preamble plus a single unlabeled sample.
fn push_metric(out: &mut String, name: &str, help: &str, kind: &str, value: &str) {
    push_metric_header(out, name, help, kind);
    out.push_str(&format!("{name} {value}\n"));
}

fn push_metric_header(out: &mut String, name: &str, help: &str, kind: &str) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
}

/// Resident set size of this process, read from `/proc/self/status`, or
/// `None` where `/proc` isn't available (non-Linux). Best-effort: a
/// metrics endpoint that omits one line is strictly better than one that
/// fails or panics over a file it couldn't read.
fn process_resident_memory_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let rest = line.strip_prefix("VmRSS:")?;
        let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
        Some(kb * 1024)
    })
}

/// Binds the probe listener. Called from `main` rather than inside the
/// spawned task so a bind failure (port already taken, misconfigured
/// environment) is a startup error, not a silently missing probe.
///
/// Address defaults to `0.0.0.0:8080` and can be overridden with
/// `HEALTH_ADDR` (e.g. `127.0.0.1:9090`), since a self-hoster may already
/// be running something else on 8080 - the README's scrape configuration
/// must simply match whichever address is chosen.
pub async fn bind() -> std::io::Result<TcpListener> {
    let addr = std::env::var("HEALTH_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(addr = %listener.local_addr()?, "health endpoint listening");
    Ok(listener)
}

/// How long a probe connection may take to deliver its request line before
/// the responder gives up on it. Five seconds is deliberately generous
/// (kubelet and vmagent send their fixed, tiny request line immediately);
/// anything slower is a broken or deliberately silent peer, and the
/// deadline's job is to make sure it costs nothing more than a closed
/// socket, never a pinned task.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);

/// Serves probe and metrics requests until the process exits. Never
/// returns on its own; run it in a spawned task.
pub async fn serve(listener: TcpListener, health: Arc<Health>, metrics: Arc<Metrics>) {
    serve_with_request_deadline(listener, health, metrics, REQUEST_DEADLINE).await
}

/// Same as [`serve`], with the per-connection request deadline injectable
/// so tests can exercise the silent-connection path without waiting out
/// the full five seconds of [`REQUEST_DEADLINE`].
async fn serve_with_request_deadline(
    listener: TcpListener,
    health: Arc<Health>,
    metrics: Arc<Metrics>,
    request_deadline: Duration,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let health = health.clone();
                let metrics = metrics.clone();
                tokio::spawn(async move {
                    if let Err(e) = respond(stream, &health, &metrics, request_deadline).await {
                        // A client hanging up mid-request lands here; not
                        // worth more than a trace line.
                        tracing::trace!(error = %e, "health connection error");
                    }
                });
            }
            Err(e) => {
                // accept() errors are transient by definition (ECONNABORTED
                // and friends); log and keep serving. A truly broken
                // listener surfaces as probes timing out instead.
                tracing::warn!(error = %e, "health accept error");
            }
        }
    }
}

async fn respond(
    mut stream: TcpStream,
    health: &Health,
    metrics: &Metrics,
    request_deadline: Duration,
) -> std::io::Result<()> {
    // Probes and scrapers send a fixed, tiny request line + headers; one
    // read of a small buffer is enough (kubelet never sends bodies here).
    // Reading exactly once and replying immediately keeps this
    // allocation-free. The read itself is deadline-bounded: a peer that
    // opens the socket and then goes silent must not pin this task and
    // the socket for the life of the process. On expiry the plainest
    // thing is also the right one - drop the stream (which closes the
    // socket) without any response.
    let mut buf = [0u8; 512];
    let n = match tokio::time::timeout(request_deadline, stream.read(&mut buf)).await {
        Ok(read) => read?,
        Err(_elapsed) => return Ok(()),
    };
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request.split_whitespace().nth(1).unwrap_or_default();

    let (status, body) = match path {
        "/healthz" => ("200 OK", "ok\n".to_string()),
        "/readyz" => match health.readiness() {
            Ok(()) => ("200 OK", "ok\n".to_string()),
            // The keys are escaped with the same rules as the /metrics
            // label: one key can smuggle in a newline via a server-chosen
            // account id, and a raw newline here would turn one 503 line
            // into response-line confusion for whatever parses the body.
            Err(disconnected) => (
                "503 Service Unavailable",
                format!(
                    "watchers disconnected: {}\n",
                    disconnected
                        .iter()
                        .map(|key| escape_prometheus_label(key).into_owned())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
        },
        "/metrics" => ("200 OK", render_metrics(health, metrics)),
        _ => ("404 Not Found", String::new()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

fn render_metrics(health: &Health, metrics: &Metrics) -> String {
    let snapshot = health.snapshot();
    let connected = snapshot.values().filter(|s| s.connected).count();
    let mut out = String::with_capacity(256 + 64 * snapshot.len());
    out.push_str(
        "# HELP jmap2telegram_watchers_registered Number of JMAP accounts currently watched.\n\
         # TYPE jmap2telegram_watchers_registered gauge\n",
    );
    out.push_str(&format!(
        "jmap2telegram_watchers_registered {}\n",
        snapshot.len()
    ));
    out.push_str(
        "# HELP jmap2telegram_watchers_connected Number of watchers with an open EventSource connection.\n\
         # TYPE jmap2telegram_watchers_connected gauge\n",
    );
    out.push_str(&format!("jmap2telegram_watchers_connected {connected}\n"));
    out.push_str(
        "# HELP jmap2telegram_last_activity_age_seconds Seconds since each watcher's last successful JMAP sync.\n\
         # TYPE jmap2telegram_last_activity_age_seconds gauge\n",
    );
    for (key, status) in &snapshot {
        // Watcher keys embed the Telegram chat id and the account slot -
        // internal identifiers, never credentials. The JMAP account id
        // part is *not* internal though (the server picks it), so the key
        // is escaped into the label value per the text format rather than
        // interpolated raw; see escape_prometheus_label. +Inf until the
        // first sync so a fresh watcher doesn't masquerade as a fresh
        // stream. Spelled "+Inf" (not Rust's "inf") per the text format.
        let age = match status.last_activity {
            Some(t) => t.elapsed().as_secs_f64().to_string(),
            None => "+Inf".to_string(),
        };
        out.push_str(&format!(
            "jmap2telegram_last_activity_age_seconds{{watcher=\"{}\"}} {age}\n",
            escape_prometheus_label(key)
        ));
    }
    metrics.render(&mut out);
    out
}

/// Escapes a string for inclusion in a Prometheus label value - and, for
/// consistency, in the /readyz 503 body naming the same watcher keys.
///
/// The text format mandates label values be quoted with `\` (`\\`), `"`
/// (`\"`) and newline (`\n`) escaped inside the quotes. Watcher keys embed
/// a JMAP account id that the *server* chooses (see
/// `watcher::shared_health_key`), not us: an account id containing any of
/// those characters would, unescaped, terminate the label early and break
/// the scrape - or, with a newline, inject whole fake metric lines into
/// the stream vmagent ingests.
///
/// Real keys are almost always plain ASCII, so the common case borrows
/// instead of allocating.
fn escape_prometheus_label(value: &str) -> Cow<'_, str> {
    if !value.contains(['\\', '"', '\n']) {
        return Cow::Borrowed(value);
    }
    // Backslash must be escaped first, or the backslashes this chain just
    // inserted for `"` and `\n` would get doubled again.
    Cow::Owned(
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn spawn_ephemeral(health: Arc<Health>) -> std::net::SocketAddr {
        spawn_ephemeral_with(health, Arc::new(Metrics::new())).await
    }

    async fn spawn_ephemeral_with(
        health: Arc<Health>,
        metrics: Arc<Metrics>,
    ) -> std::net::SocketAddr {
        // Port 0 lets the OS pick a free port, so tests never race each
        // other or a concurrently running dev instance on 8080.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, health, metrics));
        addr
    }

    async fn get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: probe\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf))
            .await
            .expect("response within timeout")
            .unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status, body)
    }

    #[tokio::test]
    async fn healthz_is_always_200_even_with_dead_watchers() {
        // Liveness must stay conservative: a broken watcher is readiness's
        // problem, restarting the pod over it would only make things worse.
        let health = Arc::new(Health::default());
        health.register("42/primary");
        let addr = spawn_ephemeral(health).await;
        assert_eq!(get(addr, "/healthz").await.0, 200);
    }

    #[tokio::test]
    async fn readyz_is_200_with_no_watchers() {
        let health = Arc::new(Health::default());
        let addr = spawn_ephemeral(health).await;
        let (status, body) = get(addr, "/readyz").await;
        assert_eq!(status, 200);
        assert_eq!(body, "ok\n");
    }

    #[tokio::test]
    async fn readyz_is_200_when_all_watchers_connected() {
        let health = Arc::new(Health::default());
        health.register("42/primary");
        health.set_connected("42/primary", true);
        health.register("42/extra-slot1");
        health.set_connected("42/extra-slot1", true);
        let addr = spawn_ephemeral(health).await;
        assert_eq!(get(addr, "/readyz").await.0, 200);
    }

    #[tokio::test]
    async fn readyz_is_503_naming_disconnected_watchers() {
        let health = Arc::new(Health::default());
        health.register("42/primary");
        health.set_connected("42/primary", true);
        health.register("42/shared-acc7");
        // registered, never connected
        let addr = spawn_ephemeral(health).await;
        let (status, body) = get(addr, "/readyz").await;
        assert_eq!(status, 503);
        assert!(body.contains("42/shared-acc7"), "body: {body}");
    }

    #[tokio::test]
    async fn readyz_recovers_when_the_watcher_reconnects() {
        let health = Arc::new(Health::default());
        health.register("42/primary");
        let addr = spawn_ephemeral(health.clone()).await;
        assert_eq!(get(addr, "/readyz").await.0, 503);
        health.set_connected("42/primary", true);
        assert_eq!(get(addr, "/readyz").await.0, 200);
    }

    #[tokio::test]
    async fn metrics_expose_counts_and_per_watcher_age() {
        let health = Arc::new(Health::default());
        health.register("42/primary");
        health.register("42/extra-slot1");
        health.set_connected("42/primary", true);
        health.touch("42/primary");
        let addr = spawn_ephemeral(health).await;
        let (status, body) = get(addr, "/metrics").await;
        assert_eq!(status, 200);
        assert!(body.contains("jmap2telegram_watchers_registered 2"));
        assert!(body.contains("jmap2telegram_watchers_connected 1"));
        assert!(
            body.contains("jmap2telegram_last_activity_age_seconds{watcher=\"42/primary\"}"),
            "body: {body}"
        );
        // A registered-but-never-synced watcher reports +Inf, not 0.
        assert!(
            body.contains("{watcher=\"42/extra-slot1\"} +Inf"),
            "body: {body}"
        );
    }

    #[tokio::test]
    async fn deregistered_watchers_disappear_from_every_signal() {
        let health = Arc::new(Health::default());
        health.register("42/primary");
        health.set_connected("42/primary", false);
        let addr = spawn_ephemeral(health.clone()).await;
        assert_eq!(get(addr, "/readyz").await.0, 503);
        health.deregister("42/primary");
        assert_eq!(get(addr, "/readyz").await.0, 200);
        let (_, body) = get(addr, "/metrics").await;
        assert!(body.contains("jmap2telegram_watchers_registered 0"));
    }

    #[tokio::test]
    async fn updates_for_unknown_watchers_do_not_resurrect_entries() {
        let health = Arc::new(Health::default());
        health.register("42/primary");
        health.deregister("42/primary");
        health.set_connected("42/primary", true);
        health.touch("42/primary");
        assert!(health.snapshot().is_empty());
    }

    #[test]
    fn deregister_chat_clears_every_target_kind_of_that_chat_and_only_that_chat() {
        // Keys exactly as watcher::spawn derives them from its targets, to
        // lock the coupling deregister_chat's "{chat_id}/" prefix scan
        // relies on (see watcher::shared_health_key / extra_health_key).
        let health = Arc::new(Health::default());
        for key in [
            crate::watcher::WatchTarget::Primary.health_key(42),
            crate::watcher::shared_health_key(42, "acc7"),
            crate::watcher::extra_health_key(42, "slot1"),
        ] {
            health.register(&key);
            health.set_connected(&key, true);
        }
        // neighbouring chat ids must not be caught by the prefix scan
        health.register("4/primary");
        health.register("43/shared-acc7");

        health.deregister_chat(42);

        let snapshot = health.snapshot();
        assert!(!snapshot.contains_key("42/primary"), "{snapshot:?}");
        assert!(!snapshot.contains_key("42/shared-acc7"), "{snapshot:?}");
        assert!(!snapshot.contains_key("42/extra-slot1"), "{snapshot:?}");
        assert!(snapshot.contains_key("4/primary"), "{snapshot:?}");
        assert!(snapshot.contains_key("43/shared-acc7"), "{snapshot:?}");
    }

    #[tokio::test]
    async fn unknown_path_is_404_with_empty_body() {
        let health = Arc::new(Health::default());
        let addr = spawn_ephemeral(health).await;
        let (status, body) = get(addr, "/nope").await;
        assert_eq!(status, 404);
        assert_eq!(body, "");
    }

    #[tokio::test]
    async fn serves_many_consecutive_probes() {
        // The kubelet opens a fresh connection per probe: make sure accept
        // + spawn keeps working across several requests.
        let health = Arc::new(Health::default());
        let addr = spawn_ephemeral(health).await;
        for _ in 0..20 {
            assert_eq!(get(addr, "/healthz").await.0, 200);
        }
    }

    #[test]
    fn escape_prometheus_label_escapes_backslash_quote_and_newline() {
        // Plain keys - the overwhelming majority - come back untouched.
        assert_eq!(escape_prometheus_label("42/shared-acc7"), "42/shared-acc7");
        // Each character the text format forbids raw inside a label value.
        assert_eq!(escape_prometheus_label(r"a\b"), r"a\\b");
        assert_eq!(escape_prometheus_label(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_prometheus_label("a\nb"), r"a\nb");
        // All three at once: backslash doubling must not re-escape the
        // backslashes inserted for the quote and the newline.
        assert_eq!(escape_prometheus_label("\\\"\n"), r#"\\\"\n"#);
        // A key that arrives *already* escaped has its backslashes doubled
        // (a label value never has an odd number of them).
        assert_eq!(escape_prometheus_label(r"a\nb"), r"a\\nb");
    }

    #[tokio::test]
    async fn metrics_escape_watcher_keys_containing_quote_backslash_and_newline() {
        // A JMAP account id is chosen by the server (see
        // watcher::shared_health_key); one containing a quote, a backslash
        // or even a newline must not terminate the label value or the
        // metric line.
        let health = Arc::new(Health::default());
        let key = String::from(r#"42/shared-a"b\c"#) + "\n" + "d";
        health.register(&key);
        let addr = spawn_ephemeral(health).await;
        let (status, body) = get(addr, "/metrics").await;
        assert_eq!(status, 200);
        let line = body
            .lines()
            .find(|l| l.contains("42/shared-a"))
            .expect("per-watcher metric line present");
        // One line, label value escaped per the text format: an unescaped
        // newline would have split it, an unescaped quote closed the label
        // early - either way this exact line would not exist.
        assert_eq!(
            line,
            r#"jmap2telegram_last_activity_age_seconds{watcher="42/shared-a\"b\\c\nd"} +Inf"#
        );
        // The sibling aggregate lines still parse as usual.
        assert!(
            body.contains("jmap2telegram_watchers_registered 1"),
            "body: {body}"
        );
    }

    #[tokio::test]
    async fn readyz_503_body_escapes_watcher_keys() {
        let health = Arc::new(Health::default());
        let key = String::from(r#"42/shared-a"b\c"#) + "\n" + "d";
        health.register(&key);
        let addr = spawn_ephemeral(health).await;
        let (status, body) = get(addr, "/readyz").await;
        assert_eq!(status, 503);
        // The raw newline inside the key must not leak into the body - it
        // stays one line.
        assert_eq!(body.lines().count(), 1, "body: {body}");
        assert_eq!(
            body.trim_end(),
            r#"watchers disconnected: 42/shared-a\"b\\c\nd"#,
            "body: {body}"
        );
    }

    #[tokio::test]
    async fn metrics_include_build_info_and_application_metrics() {
        let health = Arc::new(Health::default());
        let metrics = Arc::new(Metrics::new());
        metrics.notification_delivered("primary");
        metrics.notification_delivered("primary");
        metrics.notification_send_error("shared");
        metrics.triage_action("archive", "ok");
        metrics.mark_read("ok");
        metrics.undo("restored");
        metrics.watcher_reconnect();
        metrics.watcher_sync();
        metrics.set_accounts(3);
        metrics.set_pending_undos(1);
        let addr = spawn_ephemeral_with(health, metrics).await;
        let (status, body) = get(addr, "/metrics").await;
        assert_eq!(status, 200);
        assert!(
            body.contains(&format!(
                "jmap2telegram_build_info{{version=\"{}\"}} 1",
                env!("CARGO_PKG_VERSION")
            )),
            "body: {body}"
        );
        assert!(
            body.contains("jmap2telegram_start_time_seconds "),
            "body: {body}"
        );
        assert!(body.contains("jmap2telegram_accounts 3"), "body: {body}");
        assert!(
            body.contains("jmap2telegram_pending_undos 1"),
            "body: {body}"
        );
        assert!(
            body.contains("jmap2telegram_notifications_delivered_total{kind=\"primary\"} 2"),
            "body: {body}"
        );
        assert!(
            body.contains("jmap2telegram_notification_send_errors_total{kind=\"shared\"} 1"),
            "body: {body}"
        );
        assert!(
            body.contains(
                "jmap2telegram_triage_actions_total{action=\"archive\",outcome=\"ok\"} 1"
            ),
            "body: {body}"
        );
        assert!(
            body.contains("jmap2telegram_mark_read_total{outcome=\"ok\"} 1"),
            "body: {body}"
        );
        assert!(
            body.contains("jmap2telegram_undo_total{outcome=\"restored\"} 1"),
            "body: {body}"
        );
        assert!(
            body.contains("jmap2telegram_watcher_reconnects_total 1"),
            "body: {body}"
        );
        assert!(
            body.contains("jmap2telegram_watcher_syncs_total 1"),
            "body: {body}"
        );
        // Each family's HELP/TYPE preamble is emitted exactly once.
        assert_eq!(
            body.matches("# TYPE jmap2telegram_notifications_delivered_total counter")
                .count(),
            1,
            "body: {body}"
        );
    }

    #[tokio::test]
    async fn metrics_separate_samples_of_the_same_counter_family() {
        let health = Arc::new(Health::default());
        let metrics = Arc::new(Metrics::new());
        metrics.triage_action("archive", "ok");
        metrics.triage_action("delete", "error");
        let addr = spawn_ephemeral_with(health, metrics).await;
        let (_, body) = get(addr, "/metrics").await;
        assert!(
            body.contains(
                "jmap2telegram_triage_actions_total{action=\"archive\",outcome=\"ok\"} 1"
            ),
            "body: {body}"
        );
        assert!(
            body.contains(
                "jmap2telegram_triage_actions_total{action=\"delete\",outcome=\"error\"} 1"
            ),
            "body: {body}"
        );
    }

    #[test]
    fn process_resident_memory_bytes_is_plausible_where_proc_exists() {
        #[cfg(target_os = "linux")]
        {
            let rss = process_resident_memory_bytes().expect("VmRSS must be readable on Linux");
            assert!(rss > 0);
        }
    }

    #[tokio::test]
    async fn silent_connection_is_closed_at_the_read_deadline() {
        // A peer that connects and then never sends a byte must not pin
        // the accept-spawned task and its socket for the life of the
        // process: the deadline-bounded read gives up and dropping the
        // stream closes the socket. Production gives probes the full
        // REQUEST_DEADLINE (5s); the test injects a short deadline through
        // serve_with_request_deadline so it observes the close in
        // milliseconds instead of waiting out five seconds.
        let health = Arc::new(Health::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_with_request_deadline(
            listener,
            health,
            Arc::new(Metrics::new()),
            Duration::from_millis(150),
        ));
        let mut silent = TcpStream::connect(addr).await.unwrap();
        let mut buf = Vec::new();
        // EOF (read_to_end returning) is the observable close; the outer
        // timeout fails the test if the deadline path ever regresses.
        tokio::time::timeout(Duration::from_secs(2), silent.read_to_end(&mut buf))
            .await
            .expect("silent connection closed before the test timeout")
            .unwrap();
        assert!(buf.is_empty());
    }
}
