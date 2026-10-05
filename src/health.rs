//! Health endpoint, readiness and Prometheus metrics for Kubernetes.
//!
//! The bot has no server of its own (it's a Telegram long-poller plus JMAP
//! EventSource watchers), so this module provides the smallest possible
//! surface kubelet and vmagent can talk to: a hand-rolled HTTP/1.1
//! responder on a dedicated listener — no HTTP framework dependency, no
//! shared state that can deadlock the request path.
//!
//! Contract, deliberately split by what each signal is *for*:
//!
//! - **`/healthz` (liveness) stays conservative on purpose.** It answers
//!   200 as long as the tokio runtime is scheduling the accept loop. A
//!   false-positive liveness kill is the worst outcome for a single-replica
//!   stateful bot — it interrupts the Telegram poll loop and drops undo
//!   state — so liveness deliberately does NOT encode watcher health. A
//!   wedged runtime (deadlocked scheduler, exhausted memory) stops the
//!   accept loop and the kubelet timeout restarts the pod.
//!
//! - **`/readyz` (readiness) reflects the watchers.** 200 when every
//!   registered watcher currently holds an open EventSource connection (or
//!   when none are registered yet — boot, or a bot with no `/login` at
//!   all). A watcher stuck in reconnect backoff makes the pod NotReady.
//!   There is no Service in front of this bot, so NotReady doesn't shed
//!   traffic; what it does is surface a *silently broken* bot (revoked
//!   token, unreachable JMAP server) in `kubectl` and — via the pod Ready
//!   condition — in ArgoCD application health and the cluster's
//!   ArgoCDAppDegraded alert, without restarting anything that a backoff
//!   loop can heal on its own.
//!
//! - **`/metrics` exposes Prometheus text format** so vmagent can scrape
//!   the same registry: `jmap2telegram_watchers_registered` /
//!   `_connected` gauges and per-watcher
//!   `jmap2telegram_last_activity_age_seconds`. `last_activity` advances
//!   on a successful JMAP sync (EventSource wake-up or the 5-minute
//!   fallback poll), so an age above `FALLBACK_POLL` + margin means that
//!   watcher is neither receiving events nor completing fallback syncs —
//!   alert material, not restart material.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One watched JMAP account's live state, as tracked by its watcher task.
#[derive(Clone)]
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

    /// Records whether this watcher's EventSource connection is open.
    pub fn set_connected(&self, key: &str, connected: bool) {
        self.update(key, |s| s.connected = connected);
    }

    /// Advances the activity clock after a successful JMAP sync.
    pub fn touch(&self, key: &str) {
        self.update(key, |s| s.last_activity = Some(Instant::now()));
    }

    fn snapshot(&self) -> HashMap<String, WatcherStatus> {
        self.watchers.lock().unwrap().clone()
    }

    /// Readiness: every registered watcher connected, or nothing to watch.
    /// Returns the failing keys for the 503 body.
    fn readiness(&self) -> Result<(), Vec<String>> {
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

/// Binds the probe listener. Called from `main` rather than inside the
/// spawned task so a bind failure (port already taken, misconfigured
/// environment) is a startup error, not a silently missing probe.
pub async fn bind() -> std::io::Result<TcpListener> {
    let listener = TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!(addr = %listener.local_addr()?, "health endpoint listening");
    Ok(listener)
}

/// Serves probe and metrics requests until the process exits. Never
/// returns on its own; run it in a spawned task.
pub async fn serve(listener: TcpListener, health: Arc<Health>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let health = health.clone();
                tokio::spawn(async move {
                    if let Err(e) = respond(stream, &health).await {
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

async fn respond(mut stream: TcpStream, health: &Health) -> std::io::Result<()> {
    // Probes and scrapers send a fixed, tiny request line + headers; one
    // read of a small buffer is enough (kubelet never sends bodies here).
    // Reading exactly once and replying immediately keeps this
    // allocation-free.
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request.split_whitespace().nth(1).unwrap_or_default();

    let (status, body) = match path {
        "/healthz" => ("200 OK", "ok\n".to_string()),
        "/readyz" => match health.readiness() {
            Ok(()) => ("200 OK", "ok\n".to_string()),
            Err(disconnected) => (
                "503 Service Unavailable",
                format!("watchers disconnected: {}\n", disconnected.join(", ")),
            ),
        },
        "/metrics" => ("200 OK", render_metrics(health)),
        _ => ("404 Not Found", String::new()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

fn render_metrics(health: &Health) -> String {
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
        // Watcher keys embed the Telegram chat id and the account slot —
        // internal identifiers, never credentials. +Inf until the first
        // sync so a fresh watcher doesn't masquerade as a fresh stream.
        // Spelled "+Inf" (not Rust's "inf") per the Prometheus text format.
        let age = match status.last_activity {
            Some(t) => t.elapsed().as_secs_f64().to_string(),
            None => "+Inf".to_string(),
        };
        out.push_str(&format!(
            "jmap2telegram_last_activity_age_seconds{{watcher=\"{key}\"}} {age}\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn spawn_ephemeral(health: Arc<Health>) -> std::net::SocketAddr {
        // Port 0 lets the OS pick a free port, so tests never race each
        // other or a concurrently running dev instance on 8080.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, health));
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
}
