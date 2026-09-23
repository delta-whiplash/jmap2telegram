use std::sync::Arc;
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

        loop {
            match run_once(&bot, &state, chat_id, &client, &target).await {
                Ok(()) => {
                    // Clean shutdown requested (account removed/toggled off).
                    return;
                }
                Err(e) => {
                    tracing::warn!(chat_id, error = %e, "watcher JMAP EventSource error, reconnecting");
                }
            }

            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    })
}

async fn run_once(
    bot: &Bot,
    state: &AppState,
    chat_id: i64,
    client: &Arc<JmapClient>,
    target: &WatchTarget,
) -> anyhow::Result<()> {
    let mut stream = client
        .event_source(Some(jmap::WATCHED_TYPES), false, Some(60), None)
        .await?;

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
        let text = notification_text(summary, state.config.timezone, target.label());
        let keyboard = notification_keyboard(&summary.id, target.account_id());
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
}
