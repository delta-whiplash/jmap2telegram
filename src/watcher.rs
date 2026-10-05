use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use jmap_client::client::Client as JmapClient;
use jmap_client::event_source::PushNotification;
use teloxide::prelude::*;
use teloxide::types::ParseMode;

use crate::Bot;
use crate::format::{notification_keyboard, notification_text};
use crate::jmap;
use crate::state::AppState;

const FALLBACK_POLL: Duration = Duration::from_secs(300);

/// Backoff schedule shared by this module's reconnect loop and the
/// boot-resume retry loop in accounts.rs: double after each failure,
/// starting at 2s and never exceeding [`MAX_BACKOFF`]. Shared constants
/// (rather than two coincidentally identical schedules) so the two loops
/// stay honest about being the same policy: wait out a transient failure
/// without hammering a struggling server, and still recover within one
/// interval once it comes back.
pub(crate) const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// The next value of [`INITIAL_BACKOFF`]-seeded doubling backoff after a
/// failed attempt (2s, 4s, 8s, ..., capped at [`MAX_BACKOFF`]).
pub(crate) fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// How many *consecutive* failures to even open the EventSource connection
/// (not counting a later drop of an already-working stream, which the
/// backoff/retry loop already handles fine) before we tell the user
/// something is actually wrong - e.g. a revoked token, not just a blip.
/// With the doubling backoff below (2s, 4s, 8s, 16s, 32s, ...) this fires
/// after roughly a minute of total inability to connect.
const FAILURE_NOTIFY_THRESHOLD: u32 = 5;

/// Which JMAP account a watcher is following: the chat's own mailbox, one
/// of its opted-in shared/delegated accounts, or one of its extra, fully
/// independent personal accounts (`/comptes`). Threads through everything
/// that needs to read/write the right cursor in the store, decide when to
/// stop, and label/route notifications so an action button lands on a
/// JMAP client connected to the right account.
#[derive(Clone, Debug)]
pub enum WatchTarget {
    Primary,
    Shared { account_id: String, label: String },
    Extra { slot_id: String, label: String },
}

impl WatchTarget {
    fn since_state(&self, account: &crate::store::Account) -> Option<String> {
        match self {
            WatchTarget::Primary => account.last_state.clone(),
            WatchTarget::Shared { account_id, .. } => account
                .shared_accounts
                .get(account_id)
                .and_then(|s| s.last_state.clone()),
            WatchTarget::Extra { slot_id, .. } => account
                .extra_accounts
                .get(slot_id)
                .and_then(|e| e.last_state.clone()),
        }
    }

    /// Whether this target is still opted into, i.e. whether the watcher
    /// (or a boot-resume retrying its connect) should keep going. `false`
    /// means /logout, a /partages toggle-off, or a /comptes disconnect
    /// happened while we were waiting.
    pub(crate) fn still_active(&self, account: &crate::store::Account) -> bool {
        match self {
            WatchTarget::Primary => true,
            WatchTarget::Shared { account_id, .. } => {
                account.shared_accounts.contains_key(account_id)
            }
            WatchTarget::Extra { slot_id, .. } => account.extra_accounts.contains_key(slot_id),
        }
    }

    fn update_state(&self, store: &crate::store::Store, chat_id: i64, new_state: String) {
        let result = match self {
            WatchTarget::Primary => store.update_state(chat_id, new_state),
            WatchTarget::Shared { account_id, .. } => {
                store.update_shared_account_state(chat_id, account_id, new_state)
            }
            WatchTarget::Extra { slot_id, .. } => {
                store.update_extra_account_state(chat_id, slot_id, new_state)
            }
        };
        if let Err(e) = result {
            tracing::warn!(chat_id, error = %e, "failed to persist sync cursor");
        }
    }

    fn account_id(&self) -> Option<&str> {
        match self {
            WatchTarget::Primary => None,
            WatchTarget::Shared { account_id, .. } => Some(account_id),
            WatchTarget::Extra { slot_id, .. } => Some(slot_id),
        }
    }

    fn label(&self) -> Option<&str> {
        match self {
            WatchTarget::Primary => None,
            WatchTarget::Shared { label, .. } | WatchTarget::Extra { label, .. } => Some(label),
        }
    }

    /// Registry key for the health endpoint: stable, unique per watched
    /// account within a chat, and readable in `kubectl` output and metric
    /// labels. Embeds the chat id and the account slot - internal
    /// identifiers only, never credentials.
    pub(crate) fn health_key(&self, chat_id: i64) -> String {
        match self {
            WatchTarget::Primary => format!("{chat_id}/primary"),
            WatchTarget::Shared { account_id, .. } => format!("{chat_id}/shared-{account_id}"),
            WatchTarget::Extra { slot_id, .. } => format!("{chat_id}/extra-{slot_id}"),
        }
    }

    /// How this target is referred to in a connection-health notification.
    fn scope_label(&self) -> String {
        match self {
            WatchTarget::Primary => "ta boîte JMAP".to_string(),
            WatchTarget::Shared { label, .. } => format!("la boîte partagée « {label} »"),
            WatchTarget::Extra { label, .. } => format!("le compte « {label} »"),
        }
    }
}

/// Health-registry key for one shared account's watcher, for teardown
/// sites (`state.rs`'s `forget_shared`) that only know the slot
/// identifiers, never the display label - which plays no part in
/// [`WatchTarget::health_key`] anyway. Building the minimal target and
/// reusing the same key builder the watcher itself uses keeps the key
/// format defined in exactly one place.
pub(crate) fn shared_health_key(chat_id: i64, account_id: &str) -> String {
    WatchTarget::Shared {
        account_id: account_id.to_string(),
        label: String::new(),
    }
    .health_key(chat_id)
}

/// Same as [`shared_health_key`], for an extra account's slot
/// (`state.rs`'s `forget_extra`).
pub(crate) fn extra_health_key(chat_id: i64, slot_id: &str) -> String {
    WatchTarget::Extra {
        slot_id: slot_id.to_string(),
        label: String::new(),
    }
    .health_key(chat_id)
}

/// Connection-broken health notice. Deliberately interpolates *no*
/// runtime error at all: the last connect/stream error used to be
/// rendered here verbatim, and against a server hostname an attacker can
/// influence (see the residual DNS-rebinding TOCTOU in SECURITY.md,
/// "SSRF via /login") that verbatim transport error - jmap-client's
/// Display for it is "Transport error: <full URL + OS-level TCP cause>",
/// which is an internal-port-reachability oracle landing directly in the
/// attacker's chat. Every failure is already traced server-side by the
/// reconnect loop below, so the notice points at the logs instead of
/// quoting the error. Keep it that way: no `{}` of an error, ever.
fn connection_broken_text(target: &WatchTarget) -> String {
    format!(
        "⚠️ La connexion à {scope} échoue depuis plusieurs tentatives : le jeton a peut-être \
         été révoqué, ou le serveur est injoignable. Les notifications sont en pause pour ce \
         compte jusqu'à ce que la connexion reprenne d'elle-même.\n\n\
         Le détail technique de chaque échec est consigné dans les logs serveur.\n\n\
         Si le jeton a été révoqué, reconnecte-toi avec /login (ou /partages pour une boîte \
         partagée).",
        scope = target.scope_label(),
    )
}

fn connection_recovered_text(target: &WatchTarget) -> String {
    format!(
        "✅ La connexion à {} est rétablie, les notifications reprennent.",
        target.scope_label(),
    )
}

/// Spawns the long-running per-account watcher: an EventSource connection
/// used purely as a wake-up signal, backed by the JMAP `state` cursor as
/// the actual source of truth. A periodic fallback poll and an
/// exponential-backoff reconnect loop make it resilient to proxies that
/// silently stall SSE connections or transient network failures.
pub fn spawn(
    bot: Bot,
    state: AppState,
    chat_id: i64,
    client: Arc<JmapClient>,
    target: WatchTarget,
) -> tokio::task::JoinHandle<()> {
    let health_key = target.health_key(chat_id);
    state.health.register(&health_key);
    tokio::spawn(async move {
        let mut backoff = INITIAL_BACKOFF;
        let mut consecutive_connect_failures: u32 = 0;
        let mut notified_broken = false;
        // Set by run_once as soon as the EventSource connection actually
        // opens, so the outer loop can tell "never managed to connect this
        // whole time" (worth alerting on) apart from "connected fine, then
        // an already-open stream later dropped" (ordinary network hiccup,
        // already handled by the backoff/retry below).
        let connected = Arc::new(AtomicBool::new(false));

        loop {
            connected.store(false, Ordering::Relaxed);
            match run_once(&bot, &state, chat_id, &client, &target, &connected).await {
                Ok(()) => {
                    // Clean shutdown requested (account removed/toggled off).
                    // Dropping the registry entry here (rather than on task
                    // exit below) keeps /readyz and /metrics exact even
                    // though the JoinHandle outlives the account.
                    state.health.deregister(&health_key);
                    return;
                }
                Err(e) => {
                    tracing::warn!(chat_id, error = %e, "watcher JMAP EventSource error, reconnecting");
                    // Le stream vient de tomber (ou n'a jamais ouvert) : le
                    // registre health doit le refléter dès maintenant, sinon
                    // /readyz reste 200 pendant tout le backoff - exactement
                    // la panne silencieuse qu'il est censé signaler.
                    state.health.set_connected(&health_key, false);

                    if connected.load(Ordering::Relaxed) {
                        consecutive_connect_failures = 0;
                        backoff = INITIAL_BACKOFF;
                        if notified_broken {
                            notified_broken = false;
                            notify(&bot, &state, chat_id, connection_recovered_text(&target)).await;
                        }
                    } else {
                        consecutive_connect_failures += 1;
                        if consecutive_connect_failures >= FAILURE_NOTIFY_THRESHOLD
                            && !notified_broken
                        {
                            notified_broken = true;
                            // No error object crosses into the notice:
                            // the reconnect loop's warn! above already
                            // traced it server-side (see
                            // connection_broken_text for why the chat
                            // must never see it).
                            notify(&bot, &state, chat_id, connection_broken_text(&target)).await;
                        }
                    }
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = next_backoff(backoff);
        }
    })
}

/// Sends a connection-health notice, unless the chat has since disconnected
/// entirely (a race with /logout while we were mid-retry).
async fn notify(bot: &Bot, state: &AppState, chat_id: i64, text: String) {
    if state.store.get(chat_id).is_none() {
        return;
    }
    if let Err(e) = bot.send_message(ChatId(chat_id), text).await {
        tracing::warn!(chat_id, error = %e, "failed to deliver connection-health notice");
    }
}

async fn run_once(
    bot: &Bot,
    state: &AppState,
    chat_id: i64,
    client: &Arc<JmapClient>,
    target: &WatchTarget,
    connected: &AtomicBool,
) -> anyhow::Result<()> {
    let mut stream = client
        .event_source(Some(jmap::WATCHED_TYPES), false, Some(60), None)
        .await?;
    connected.store(true, Ordering::Relaxed);
    state
        .health
        .set_connected(&target.health_key(chat_id), true);

    let mut interval = tokio::time::interval(FALLBACK_POLL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // Still authorized / still opted in? Stop cleanly if the account
        // was removed (/logout) or this shared account was toggled off
        // (/partages) while we were waiting.
        match state.store.get(chat_id) {
            Some(account) if target.still_active(&account) => {}
            _ => return Ok(()),
        }

        tokio::select! {
            event = stream.next() => {
                match event {
                    Some(Ok(PushNotification::StateChange(_))) => {
                        sync_and_notify(bot, state, chat_id, client, target).await;
                    }
                    Some(Ok(PushNotification::CalendarAlert(_))) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Err(anyhow::anyhow!("EventSource stream closed")),
                }
            }
            _ = interval.tick() => {
                sync_and_notify(bot, state, chat_id, client, target).await;
            }
        }
    }
}

async fn sync_and_notify(
    bot: &Bot,
    state: &AppState,
    chat_id: i64,
    client: &Arc<JmapClient>,
    target: &WatchTarget,
) {
    let Some(account) = state.store.get(chat_id) else {
        return;
    };
    let Some(since_state) = target.since_state(&account) else {
        // Should not normally happen (set right after opting in), but
        // guard against notifying the user's entire mailbox history.
        if let Ok(fresh_state) = jmap::current_email_state(client).await {
            target.update_state(&state.store, chat_id, fresh_state);
        }
        return;
    };

    let (summaries, new_state) = match jmap::fetch_changed_emails(client, &since_state).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(chat_id, error = %e, "failed to sync new emails");
            return;
        }
    };
    // Successful sync (event-driven or fallback poll): advance the health
    // activity clock behind jmap2telegram_last_activity_age_seconds.
    state.health.touch(&target.health_key(chat_id));

    for summary in &summaries {
        if is_muted(summary, &account.muted) {
            continue;
        }

        let text = notification_text(summary, state.config.timezone, target.label());
        let keyboard = notification_keyboard(&summary.id, target.account_id(), false);
        if let Err(e) = bot
            .send_message(ChatId(chat_id), text)
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(keyboard)
            .await
        {
            tracing::warn!(chat_id, error = %e, "failed to deliver notification");
        }
    }

    if new_state != since_state {
        target.update_state(&state.store, chat_id, new_state);
    }
}

/// Whether a `/mute`d term (already lowercased) appears in this message's
/// subject or sender (name or address). Matching is deliberately broad
/// (substring, case-insensitive) rather than exact-address matching, so
/// `/mute newsletter` also catches `newsletter@example.org` and a subject
/// containing "Newsletter" - the same trade-off GmailBot's own Blacklist
/// makes. The JMAP sync cursor still advances past a muted message; this
/// only decides whether to notify, not whether to see it as unread later.
fn is_muted(summary: &jmap::EmailSummary, muted: &[String]) -> bool {
    if muted.is_empty() {
        return false;
    }
    let haystack = format!(
        "{} {} {}",
        summary.subject.to_lowercase(),
        summary
            .from_name
            .as_deref()
            .unwrap_or_default()
            .to_lowercase(),
        summary
            .from_addr
            .as_deref()
            .unwrap_or_default()
            .to_lowercase(),
    );
    muted.iter().any(|term| haystack.contains(term.as_str()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::store::{Account, SharedAccount};

    fn account() -> Account {
        Account {
            server_url: "https://jmap.example.org".to_string(),
            token: "tok".to_string(),
            email: "a@b.c".to_string(),
            last_state: Some("primary-state".to_string()),
            shared_accounts: HashMap::from([(
                "acc7".to_string(),
                SharedAccount {
                    name: "shared@b.c".to_string(),
                    last_state: Some("shared-state".to_string()),
                },
            )]),
            extra_accounts: HashMap::from([(
                "slot1".to_string(),
                crate::store::ExtraAccount {
                    server_url: "https://jmap.other.example".to_string(),
                    token: "other-tok".to_string(),
                    email: "second@other.example".to_string(),
                    last_state: Some("extra-state".to_string()),
                },
            )]),
            muted: Vec::new(),
        }
    }

    #[test]
    fn primary_target_reads_the_account_level_cursor() {
        let target = WatchTarget::Primary;
        assert_eq!(
            target.since_state(&account()),
            Some("primary-state".to_string())
        );
        assert!(target.still_active(&account()));
        assert_eq!(target.account_id(), None);
        assert_eq!(target.label(), None);
    }

    #[test]
    fn shared_target_reads_its_own_cursor_and_label() {
        let target = WatchTarget::Shared {
            account_id: "acc7".to_string(),
            label: "shared@b.c".to_string(),
        };
        assert_eq!(
            target.since_state(&account()),
            Some("shared-state".to_string())
        );
        assert!(target.still_active(&account()));
        assert_eq!(target.account_id(), Some("acc7"));
        assert_eq!(target.label(), Some("shared@b.c"));
    }

    #[test]
    fn shared_target_stops_being_active_once_toggled_off() {
        // Simulates /partages toggling this shared account off: it's no
        // longer in the store's shared_accounts map, so the watcher must
        // exit instead of continuing to poll/notify.
        let target = WatchTarget::Shared {
            account_id: "does-not-exist".to_string(),
            label: "gone@b.c".to_string(),
        };
        assert!(!target.still_active(&account()));
        assert_eq!(target.since_state(&account()), None);
    }

    #[test]
    fn extra_target_reads_its_own_cursor_and_label() {
        let target = WatchTarget::Extra {
            slot_id: "slot1".to_string(),
            label: "second@other.example".to_string(),
        };
        assert_eq!(
            target.since_state(&account()),
            Some("extra-state".to_string())
        );
        assert!(target.still_active(&account()));
        assert_eq!(target.account_id(), Some("slot1"));
        assert_eq!(target.label(), Some("second@other.example"));
    }

    #[test]
    fn extra_target_stops_being_active_once_disconnected() {
        // Simulates /comptes disconnecting this extra account: no longer
        // in the store's extra_accounts map, so the watcher must exit.
        let target = WatchTarget::Extra {
            slot_id: "does-not-exist".to_string(),
            label: "gone@other.example".to_string(),
        };
        assert!(!target.still_active(&account()));
        assert_eq!(target.since_state(&account()), None);
    }

    #[test]
    fn extra_and_shared_account_ids_never_share_a_key_space() {
        // Sanity check for the design invariant the whole feature leans
        // on: a shared account's routing key ("acc7") and an extra
        // account's slot id ("slot1") live in genuinely separate store
        // maps, so a since_state lookup for one never accidentally
        // resolves against the other.
        let acc = account();
        assert!(acc.shared_accounts.contains_key("acc7"));
        assert!(!acc.extra_accounts.contains_key("acc7"));
        assert!(acc.extra_accounts.contains_key("slot1"));
        assert!(!acc.shared_accounts.contains_key("slot1"));
    }

    #[test]
    fn health_keys_use_a_stable_slash_separated_format() {
        // The exact format matters beyond readability: health.rs's
        // deregister_chat strips entries by "{chat_id}/" prefix and the
        // keys end up in /readyz bodies, kubectl output and Prometheus
        // labels, so a change is user-visible and must be deliberate.
        assert_eq!(WatchTarget::Primary.health_key(42), "42/primary");
        assert_eq!(shared_health_key(42, "acc7"), "42/shared-acc7");
        assert_eq!(extra_health_key(42, "slot1"), "42/extra-slot1");
        // The teardown helpers must derive exactly the key a real watcher
        // registers, including when the label differs - the label never
        // participates in the key.
        assert_eq!(
            shared_health_key(42, "acc7"),
            WatchTarget::Shared {
                account_id: "acc7".to_string(),
                label: "contact@delta-net.ovh".to_string(),
            }
            .health_key(42)
        );
        assert_eq!(
            extra_health_key(42, "slot1"),
            WatchTarget::Extra {
                slot_id: "slot1".to_string(),
                label: "second@other.example".to_string(),
            }
            .health_key(42)
        );
    }

    #[test]
    fn health_keys_keep_neighbouring_chats_prefix_distinct() {
        // deregister_chat in health.rs matches keys by "{chat_id}/"
        // prefix; the trailing separator is what makes that exact, so
        // chat 4's teardown must never sweep chat 42's entries (or the
        // other way around).
        assert!(WatchTarget::Primary.health_key(4).starts_with("4/"));
        assert!(!WatchTarget::Primary.health_key(42).starts_with("4/"));
        assert_ne!(
            WatchTarget::Primary.health_key(4),
            WatchTarget::Primary.health_key(42)
        );
    }

    #[test]
    fn next_backoff_doubles_from_2s_and_caps_at_300s() {
        // The exact schedule the watcher reconnect loop and the boot-resume
        // retry loop share (see INITIAL_BACKOFF): 2s, 4s, ..., doubling,
        // never past MAX_BACKOFF.
        let mut backoff = INITIAL_BACKOFF;
        assert_eq!(backoff, Duration::from_secs(2));
        assert_eq!(next_backoff(backoff), Duration::from_secs(4));
        for _ in 0..20 {
            backoff = next_backoff(backoff);
        }
        assert_eq!(backoff, MAX_BACKOFF);
        assert_eq!(MAX_BACKOFF, Duration::from_secs(300));
    }

    #[test]
    fn connection_broken_text_names_the_primary_mailbox_and_points_at_the_logs() {
        let text = connection_broken_text(&WatchTarget::Primary);
        assert!(text.contains("ta boîte JMAP"));
        assert!(text.contains("logs serveur"));
        assert!(text.contains("/login"));
    }

    #[test]
    fn connection_broken_text_names_the_shared_mailbox() {
        let target = WatchTarget::Shared {
            account_id: "acc7".to_string(),
            label: "contact@delta-net.ovh".to_string(),
        };
        let text = connection_broken_text(&target);
        assert!(text.contains("contact@delta-net.ovh"));
        assert!(text.contains("logs serveur"));
    }

    /// Regression test for the SSRF error oracle: the "connection broken"
    /// notice used to interpolate the last runtime error verbatim, and
    /// that error is the raw jmap-client transport error (its Display is
    /// "Transport error: <full URL + OS-level TCP cause>"), which -
    /// especially after the DNS-rebinding TOCTOU documented in
    /// SECURITY.md - reads back as a port-state fingerprint of whatever
    /// the watched account's hostname currently resolves to. The notice
    /// must interpolate no error at all; the details live in the
    /// server-side tracing::warn of the reconnect loop.
    #[test]
    fn connection_broken_text_never_interpolates_the_transport_error() {
        let text = connection_broken_text(&WatchTarget::Primary);
        // Fragments of a plausible raw transport error, exactly what the
        // notice used to quote, plus the old quoting line itself.
        for fragment in [
            "Dernière erreur",
            "Connection refused",
            "Transport error",
            "error sending request",
            "os error",
            "timed out",
            "401",
            "internal-host",
        ] {
            assert!(
                !text.contains(fragment),
                "error-oracle leak via {fragment:?}: {text}"
            );
        }
    }

    #[test]
    fn connection_recovered_text_names_the_target() {
        let primary = connection_recovered_text(&WatchTarget::Primary);
        assert!(primary.contains("ta boîte JMAP"));

        let shared = connection_recovered_text(&WatchTarget::Shared {
            account_id: "acc7".to_string(),
            label: "contact@delta-net.ovh".to_string(),
        });
        assert!(shared.contains("contact@delta-net.ovh"));

        let extra = connection_recovered_text(&WatchTarget::Extra {
            slot_id: "slot1".to_string(),
            label: "second@other.example".to_string(),
        });
        assert!(extra.contains("second@other.example"));
    }

    fn summary(
        subject: &str,
        from_name: Option<&str>,
        from_addr: Option<&str>,
    ) -> jmap::EmailSummary {
        jmap::EmailSummary {
            id: "M1".to_string(),
            subject: subject.to_string(),
            from_name: from_name.map(str::to_string),
            from_addr: from_addr.map(str::to_string),
            preview: String::new(),
            received_at: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn is_muted_matches_subject_case_insensitively() {
        let s = summary("Big Newsletter Blast", None, Some("a@b.c"));
        assert!(is_muted(&s, &["newsletter".to_string()]));
        assert!(!is_muted(&s, &["invoice".to_string()]));
    }

    #[test]
    fn is_muted_matches_sender_name_or_address() {
        let s = summary("Hello", Some("Marketing Team"), Some("promo@shop.example"));
        assert!(is_muted(&s, &["marketing".to_string()]));
        assert!(is_muted(&s, &["shop.example".to_string()]));
    }

    #[test]
    fn is_muted_is_false_with_no_filters_or_no_match() {
        let s = summary("Hello", Some("Alice"), Some("alice@example.org"));
        assert!(!is_muted(&s, &[]));
        assert!(!is_muted(&s, &["bob".to_string()]));
    }
}
