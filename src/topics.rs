use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId, Rgb, ThreadId};

use crate::Bot;
use crate::config::{Config, SharedThreadModeConfig, ThreadStrategyConfig, ThreadedModeConfig};
use crate::state::AppState;
use crate::store::Account;
use crate::watcher::WatchTarget;

/// Telegram allowed RGB colors for forum topic icons:
/// - Blue: 7322096 (0x6FB9F0)
/// - Yellow: 16766590 (0xFFD67E)
/// - Purple: 13338331 (0xCB86DB)
/// - Green: 9367192 (0x8EEE98)
/// - Pink: 16749490 (0xFF93B2)
/// - Red: 16478047 (0xFB6F5F)
pub const TOPIC_COLORS: [u32; 6] = [
    7322096,  // Blue
    16766590, // Yellow
    13338331, // Purple
    9367192,  // Green
    16749490, // Pink
    16478047, // Red
];

/// Picks a deterministic topic color based on a seed string (e.g. account email or slot ID).
pub fn pick_topic_color(seed: &str) -> u32 {
    let mut hash: u32 = 5381;
    for byte in seed.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(byte as u32);
    }
    TOPIC_COLORS[(hash as usize) % TOPIC_COLORS.len()]
}

/// Truncates a topic name to fit Telegram's 1-128 character limit safely on a char boundary.
pub fn format_topic_name(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.chars().count() <= 128 {
        return trimmed.to_string();
    }
    trimmed.chars().take(128).collect()
}

/// Formats the topic name for an account slot according to the configured strategy.
pub fn slot_topic_name(strategy: ThreadStrategyConfig, email: &str, server_url: &str) -> String {
    match strategy {
        ThreadStrategyConfig::Server => {
            let host = url::Url::parse(server_url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_else(|| server_url.to_string());
            format_topic_name(&format!("🌐 {host}"))
        }
        ThreadStrategyConfig::Account => format_topic_name(&format!("📧 {email}")),
    }
}

/// Formats the topic name for a dedicated shared mailbox.
pub fn shared_topic_name(name: &str) -> String {
    format_topic_name(&format!("👥 {name}"))
}

/// Determines whether threaded mode (topics) should be used for a given chat.
pub fn is_threaded_mode_enabled(config: &Config, account: Option<&Account>) -> bool {
    match account.and_then(|a| a.threads_enabled) {
        Some(user_override) => user_override,
        None => match config.telegram_threaded_mode {
            ThreadedModeConfig::Disabled => false,
            ThreadedModeConfig::Enabled | ThreadedModeConfig::Auto => true,
        },
    }
}

/// Returns true if the Telegram error indicates the chat is not a forum or topics are unsupported.
pub fn is_not_a_forum_error(err: &teloxide::RequestError) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("not a forum")
        || msg.contains("chat does not support topics")
        || msg.contains("topics are not enabled")
}

/// Returns true if the Telegram error indicates the specific message thread was deleted or not found.
pub fn is_thread_not_found_error(err: &teloxide::RequestError) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("message thread not found")
        || msg.contains("thread not found")
        || msg.contains("topic_deleted")
}

/// Ensures a forum topic exists for the given account slot.
///
/// Idempotent: if a topic ID is already registered in `AccountSlot::thread_id`,
/// it returns that thread ID directly. If not, it attempts to create the topic.
/// If the chat does not support topics (e.g. Threaded Mode is disabled in BotFather),
/// it returns `Ok(None)` without error so normal delivery continues in the main chat.
pub async fn ensure_slot_topic(
    bot: &Bot,
    state: &AppState,
    chat_id: i64,
    slot_id: &str,
) -> Result<Option<ThreadId>> {
    let Some(account) = state.store.get(chat_id) else {
        return Ok(None);
    };

    if !is_threaded_mode_enabled(&state.config, Some(&account)) {
        return Ok(None);
    }

    let Some(slot) = account.accounts.get(slot_id) else {
        return Ok(None);
    };

    if let Some(tid) = slot.thread_id {
        return Ok(Some(ThreadId(MessageId(tid))));
    }

    let name = slot_topic_name(
        state.config.telegram_thread_strategy,
        &slot.email,
        &slot.server_url,
    );
    let color = pick_topic_color(&slot.email);

    tracing::info!(chat_id, slot_id, topic_name = %name, "creating forum topic for account");

    match bot
        .create_forum_topic(ChatId(chat_id), &name)
        .icon_color(Rgb::from_u32(color))
        .await
    {
        Ok(topic) => {
            let tid = topic.thread_id.0.0;
            state
                .store
                .set_account_thread_id(chat_id, slot_id, Some(tid))?;
            tracing::info!(
                chat_id,
                slot_id,
                thread_id = tid,
                "forum topic created successfully"
            );

            // Post an introductory message into the thread
            let lang = state.language_for(chat_id, None);
            let intro = t!("topics.welcome_slot", locale = lang, email = slot.email);
            let _ = bot
                .send_message(ChatId(chat_id), intro)
                .message_thread_id(topic.thread_id)
                .await;

            Ok(Some(topic.thread_id))
        }
        Err(e) if is_not_a_forum_error(&e) => {
            tracing::debug!(
                chat_id,
                error = %e,
                "chat is not a forum; skipping topic creation"
            );
            Ok(None)
        }
        Err(e) => {
            tracing::warn!(
                chat_id,
                slot_id,
                error = %e,
                "failed to create forum topic for account slot"
            );
            Ok(None)
        }
    }
}

/// Ensures a forum topic exists for a dedicated shared mailbox (if configured).
pub async fn ensure_shared_topic(
    bot: &Bot,
    state: &AppState,
    chat_id: i64,
    account_id: &str,
) -> Result<Option<ThreadId>> {
    if state.config.telegram_shared_thread_mode != SharedThreadModeConfig::Dedicated {
        return Ok(None);
    }

    let Some(account) = state.store.get(chat_id) else {
        return Ok(None);
    };

    if !is_threaded_mode_enabled(&state.config, Some(&account)) {
        return Ok(None);
    }

    let Some(shared) = account.shared_accounts.get(account_id) else {
        return Ok(None);
    };

    if let Some(tid) = shared.thread_id {
        return Ok(Some(ThreadId(MessageId(tid))));
    }

    let name = shared_topic_name(&shared.name);
    let color = pick_topic_color(&shared.name);

    tracing::info!(chat_id, account_id, topic_name = %name, "creating forum topic for shared mailbox");

    match bot
        .create_forum_topic(ChatId(chat_id), &name)
        .icon_color(Rgb::from_u32(color))
        .await
    {
        Ok(topic) => {
            let tid = topic.thread_id.0.0;
            state
                .store
                .set_shared_account_thread_id(chat_id, account_id, Some(tid))?;
            tracing::info!(
                chat_id,
                account_id,
                thread_id = tid,
                "shared mailbox forum topic created"
            );

            let lang = state.language_for(chat_id, None);
            let intro = t!("topics.welcome_shared", locale = lang, name = shared.name);
            let _ = bot
                .send_message(ChatId(chat_id), intro)
                .message_thread_id(topic.thread_id)
                .await;

            Ok(Some(topic.thread_id))
        }
        Err(e) if is_not_a_forum_error(&e) => {
            tracing::debug!(
                chat_id,
                error = %e,
                "chat is not a forum; skipping shared topic creation"
            );
            Ok(None)
        }
        Err(e) => {
            tracing::warn!(
                chat_id,
                account_id,
                error = %e,
                "failed to create forum topic for shared account"
            );
            Ok(None)
        }
    }
}

/// Deletes a forum topic when an account or shared mailbox is disconnected.
pub async fn delete_topic(bot: &Bot, chat_id: i64, thread_id: Option<i32>) {
    let Some(tid) = thread_id else {
        return;
    };
    if let Err(e) = bot
        .delete_forum_topic(ChatId(chat_id), ThreadId(MessageId(tid)))
        .await
    {
        tracing::debug!(
            chat_id,
            thread_id = tid,
            error = %e,
            "failed to delete forum topic (may already have been deleted)"
        );
    }
}

/// Resolves the destination `ThreadId` for a `WatchTarget` based on stored configuration.
pub fn target_thread_id(
    config: &Config,
    account: &Account,
    target: &WatchTarget,
) -> Option<ThreadId> {
    if !is_threaded_mode_enabled(config, Some(account)) {
        return None;
    }

    match target {
        WatchTarget::Primary => account
            .primary_slot()
            .and_then(|s| s.thread_id)
            .map(|tid| ThreadId(MessageId(tid))),
        WatchTarget::Account { slot_id, .. } | WatchTarget::Extra { slot_id, .. } => account
            .accounts
            .get(slot_id)
            .and_then(|s| s.thread_id)
            .map(|tid| ThreadId(MessageId(tid))),
        WatchTarget::Shared { account_id, .. } => {
            if config.telegram_shared_thread_mode == SharedThreadModeConfig::Dedicated {
                account
                    .shared_accounts
                    .get(account_id)
                    .and_then(|s| s.thread_id)
                    .map(|tid| ThreadId(MessageId(tid)))
            } else {
                // Shared mailboxes route into their parent account's topic by default
                let parent_slot = account
                    .shared_accounts
                    .get(account_id)
                    .and_then(|s| account.resolve_parent_slot(s));
                parent_slot
                    .and_then(|s| s.thread_id)
                    .map(|tid| ThreadId(MessageId(tid)))
            }
        }
    }
}

/// Synchronizes / creates topics for all connected accounts of a chat.
pub async fn sync_all_topics(
    bot: &Bot,
    state: &AppState,
    chat_id: i64,
) -> Result<Vec<(String, Option<ThreadId>)>> {
    let Some(account) = state.store.get(chat_id) else {
        return Ok(Vec::new());
    };

    let mut results = Vec::new();
    for slot_id in account.accounts.keys() {
        let tid = ensure_slot_topic(bot, state, chat_id, slot_id).await?;
        results.push((slot_id.clone(), tid));
    }

    if state.config.telegram_shared_thread_mode == SharedThreadModeConfig::Dedicated {
        for account_id in account.shared_accounts.keys() {
            let tid = ensure_shared_topic(bot, state, chat_id, account_id).await?;
            results.push((account_id.clone(), tid));
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_color_is_deterministic_and_valid() {
        let c1 = pick_topic_color("alice@example.com");
        let c2 = pick_topic_color("alice@example.com");
        assert_eq!(c1, c2);
        assert!(TOPIC_COLORS.contains(&c1));

        let c3 = pick_topic_color("bob@example.com");
        assert!(TOPIC_COLORS.contains(&c3));
    }

    #[test]
    fn topic_name_formatting_and_truncation() {
        let short = slot_topic_name(
            ThreadStrategyConfig::Account,
            "alice@work.com",
            "https://jmap.work.com",
        );
        assert_eq!(short, "📧 alice@work.com");

        let server_name = slot_topic_name(
            ThreadStrategyConfig::Server,
            "alice@work.com",
            "https://mail.company.org/jmap",
        );
        assert_eq!(server_name, "🌐 mail.company.org");

        let long = "a".repeat(200);
        let formatted = format_topic_name(&long);
        assert_eq!(formatted.chars().count(), 128);
    }

    #[test]
    fn threaded_mode_enabled_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::for_testing(dir.path(), 42);

        let mut acc = Account::new(
            "https://jmap.example.org".to_string(),
            "tok".to_string(),
            "user@example.com".to_string(),
        );

        // Auto mode, no user pref -> true
        config.telegram_threaded_mode = ThreadedModeConfig::Auto;
        assert!(is_threaded_mode_enabled(&config, Some(&acc)));

        // User preference overrides config
        acc.threads_enabled = Some(false);
        assert!(!is_threaded_mode_enabled(&config, Some(&acc)));

        acc.threads_enabled = Some(true);
        config.telegram_threaded_mode = ThreadedModeConfig::Disabled;
        assert!(is_threaded_mode_enabled(&config, Some(&acc)));

        // User pref None, Disabled config -> false
        acc.threads_enabled = None;
        assert!(!is_threaded_mode_enabled(&config, Some(&acc)));
    }

    #[test]
    fn target_thread_id_routing() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::for_testing(dir.path(), 42);

        let mut acc = Account::new(
            "https://jmap.example.org".to_string(),
            "tok".to_string(),
            "user@example.com".to_string(),
        );
        acc.primary_slot_mut().unwrap().thread_id = Some(101);

        // Primary target routes to default slot's thread
        let tid = target_thread_id(&config, &acc, &WatchTarget::Primary);
        assert_eq!(tid, Some(ThreadId(MessageId(101))));

        // Account target routes to specified slot's thread
        let tid_account = target_thread_id(
            &config,
            &acc,
            &WatchTarget::Account {
                slot_id: "default".to_string(),
                label: "user@example.com".to_string(),
            },
        );
        assert_eq!(tid_account, Some(ThreadId(MessageId(101))));

        // Shared target routes to parent slot's thread by default
        acc.shared_accounts.insert(
            "shared1".to_string(),
            crate::store::SharedAccount {
                name: "team@example.com".to_string(),
                last_state: None,
                parent_slot: Some("default".to_string()),
                thread_id: None,
            },
        );
        let tid_shared = target_thread_id(
            &config,
            &acc,
            &WatchTarget::Shared {
                account_id: "shared1".to_string(),
                label: "team@example.com".to_string(),
            },
        );
        assert_eq!(tid_shared, Some(ThreadId(MessageId(101))));
    }
}
