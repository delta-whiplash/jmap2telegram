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
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// How many *consecutive* failures to even open the EventSource connection
/// (not counting a later drop of an already-working stream, which the
/// backoff/retry loop already handles fine) before we tell the user
/// something is actually wrong — e.g. a revoked token, not just a blip.
/// With the doubling backoff below (2s, 4s, 8s, 16s, 32s, ...) this fires
/// after roughly a minute of total inability to connect.
const FAILURE_NOTIFY_THRESHOLD: u32 = 5;

/// Which JMAP account a watcher is following: the chat's own mailbox, or
/// one of its opted-in shared accounts. Threads through everything that
/// needs to read/write the right cursor in the store, decide when to stop,
/// and label/route notifications so an action button lands on a JMAP
/// client connected to the right account.
#[derive(Clone)]
pub enum WatchTarget {
    Primary,
    Shared { account_id: String, label: String },
}

impl WatchTarget {
    fn since_state(&self, account: &crate::store::Account) -> Option<String> {
        match self {
            WatchTarget::Primary => account.last_state.clone(),
            WatchTarget::Shared { account_id, .. } => account
                .shared_accounts
                .get(account_id)
                .and_then(|s| s.last_state.clone()),
        }
    }

    /// Whether this target is still opted into, i.e. whether the watcher
    /// should keep running. `false` means /logout or a /partages toggle-off
    /// happened while we were waiting.
    fn still_active(&self, account: &crate::store::Account) -> bool {
        match self {
            WatchTarget::Primary => true,
            WatchTarget::Shared { account_id, .. } => {
                account.shared_accounts.contains_key(account_id)
            }
        }
    }

    fn update_state(&self, store: &crate::store::Store, chat_id: i64, new_state: String) {
        let result = match self {
            WatchTarget::Primary => store.update_state(chat_id, new_state),
            WatchTarget::Shared { account_id, .. } => {
                store.update_shared_account_state(chat_id, account_id, new_state)
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
        }
    }

    fn label(&self) -> Option<&str> {
        match self {
            WatchTarget::Primary => None,
            WatchTarget::Shared { label, .. } => Some(label),
        }
    }

    /// How this target is referred to in a connection-health notification.
    fn scope_label(&self) -> String {
        match self.label() {
            Some(label) => format!("la boîte partagée « {label} »"),
            None => "ta boîte JMAP".to_string(),
        }
    }
}

fn connection_broken_text(target: &WatchTarget, last_error: &anyhow::Error) -> String {
    format!(
        "⚠️ La connexion à {scope} échoue depuis plusieurs tentatives : le jeton a peut-être \
         été révoqué, ou le serveur est injoignable. Les notifications sont en pause pour ce \
         compte jusqu'à ce que la connexion reprenne d'elle-même.\n\n\
         Dernière erreur : {last_error}\n\n\
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
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(2);
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
                    return;
                }
                Err(e) => {
                    tracing::warn!(chat_id, error = %e, "watcher JMAP EventSource error, reconnecting");

                    if connected.load(Ordering::Relaxed) {
                        consecutive_connect_failures = 0;
                        backoff = Duration::from_secs(2);
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
                            notify(&bot, &state, chat_id, connection_broken_text(&target, &e))
                                .await;
                        }
                    }
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
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
/// containing "Newsletter" — the same trade-off GmailBot's own Blacklist
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
    fn connection_broken_text_names_the_primary_mailbox_and_the_error() {
        let err = anyhow::anyhow!("401 Unauthorized");
        let text = connection_broken_text(&WatchTarget::Primary, &err);
        assert!(text.contains("ta boîte JMAP"));
        assert!(text.contains("401 Unauthorized"));
        assert!(text.contains("/login"));
    }

    #[test]
    fn connection_broken_text_names_the_shared_mailbox() {
        let err = anyhow::anyhow!("connection refused");
        let target = WatchTarget::Shared {
            account_id: "acc7".to_string(),
            label: "contact@delta-net.ovh".to_string(),
        };
        let text = connection_broken_text(&target, &err);
        assert!(text.contains("contact@delta-net.ovh"));
        assert!(text.contains("connection refused"));
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
