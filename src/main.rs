mod bot;
mod config;
mod format;
mod jmap;
mod logging;
mod state;
mod store;
mod watcher;

use std::sync::Arc;

use teloxide::adaptors::throttle::Limits;
use teloxide::prelude::*;
use teloxide::requests::RequesterExt;

use config::Config;
use state::AppState;
use store::Store;

/// Every Telegram request goes through here, not a bare `teloxide::Bot`:
/// Telegram enforces per-chat (1 msg/s) and overall (30 msg/s) rate
/// limits, and a chat that receives a burst of notifications (a mailing
/// list flood, a newsletter blast) would otherwise start getting
/// `RetryAfter` errors with no retry logic of our own to handle them.
/// `Limits::default()` matches Telegram's own documented defaults.
pub type Bot = teloxide::adaptors::Throttle<teloxide::Bot>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Timezone is needed before `Config` is fully loaded (to time-stamp
    // even the earliest log lines), so it's parsed once here and again
    // inside `Config::parse` — both go through the same validated helper.
    let early_timezone = config::parse_timezone(std::env::var("TIMEZONE").ok().as_deref())
        .unwrap_or(chrono_tz::Tz::UTC);
    logging::init(std::env::var("LOG_LEVEL").ok().as_deref(), early_timezone);

    let config = Arc::new(Config::from_env()?);
    let store = Arc::new(Store::open(&config.data_dir)?);
    let state = AppState::new(config.clone(), store.clone());

    let bot = teloxide::Bot::new(&config.telegram_token).throttle(Limits::default());

    // Resume watching every account that survived a restart, without
    // re-notifying about anything already seen (last_state is resumed
    // from the encrypted store).
    for (chat_id, account) in store.all() {
        match jmap::connect(
            &account.server_url,
            &account.token,
            config.allow_private_jmap_hosts,
        )
        .await
        {
            Ok(client) => {
                let client = Arc::new(client);
                state.clients.write().await.insert(chat_id, client.clone());
                let handle = watcher::spawn(bot.clone(), state.clone(), chat_id, client);
                state.watchers.write().await.insert(chat_id, handle);
                tracing::info!(chat_id, email = %account.email, "resumed JMAP watcher");
            }
            Err(e) => {
                tracing::warn!(chat_id, error = %e, "failed to resume JMAP account, it will need /login again");
            }
        }
    }

    tracing::info!("jmap2telegram starting");

    Dispatcher::builder(bot, bot::schema())
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}
