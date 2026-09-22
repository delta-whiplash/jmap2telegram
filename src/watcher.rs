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
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(2);

        loop {
            match run_once(&bot, &state, chat_id, &client).await {
                Ok(()) => {
                    // Clean shutdown requested (account removed).
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
) -> anyhow::Result<()> {
    let mut stream = client
        .event_source(Some(jmap::WATCHED_TYPES), false, Some(60), None)
        .await?;

    let mut interval = tokio::time::interval(FALLBACK_POLL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // Still authorized / still connected? Stop cleanly if the account
        // was removed (e.g. via /logout) while we were waiting.
        if state.store.get(chat_id).is_none() {
            return Ok(());
        }

        tokio::select! {
            event = stream.next() => {
                match event {
                    Some(Ok(PushNotification::StateChange(_))) => {
                        sync_and_notify(bot, state, chat_id, client).await;
                    }
                    Some(Ok(PushNotification::CalendarAlert(_))) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Err(anyhow::anyhow!("EventSource stream closed")),
                }
            }
            _ = interval.tick() => {
                sync_and_notify(bot, state, chat_id, client).await;
            }
        }
    }
}

async fn sync_and_notify(bot: &Bot, state: &AppState, chat_id: i64, client: &Arc<JmapClient>) {
    let Some(account) = state.store.get(chat_id) else {
        return;
    };
    let Some(since_state) = account.last_state else {
        // Should not normally happen (set right after /login), but guard
        // against notifying the user's entire mailbox history.
        if let Ok(fresh_state) = jmap::current_email_state(client).await {
            let _ = state.store.update_state(chat_id, fresh_state);
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
        let text = notification_text(summary, state.config.timezone);
        let keyboard = notification_keyboard(&summary.id);
        if let Err(e) = bot
            .send_message(ChatId(chat_id), text)
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(keyboard)
            .await
        {
            tracing::warn!(chat_id, error = %e, "failed to deliver notification");
        }
    }

    if new_state != since_state
        && let Err(e) = state.store.update_state(chat_id, new_state)
    {
        tracing::warn!(chat_id, error = %e, "failed to persist sync cursor");
    }
}
