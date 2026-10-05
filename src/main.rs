mod accounts;
mod bot;
mod config;
mod format;
mod health;
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
    // inside `Config::parse` - both go through the same validated helper.
    let early_timezone = config::parse_timezone(std::env::var("TIMEZONE").ok().as_deref())
        .unwrap_or(chrono_tz::Tz::UTC);
    logging::init(std::env::var("LOG_LEVEL").ok().as_deref(), early_timezone);

    let config = Arc::new(Config::from_env()?);
    let store = Arc::new(Store::open(&config.data_dir)?);
    let state = AppState::new(config.clone(), store.clone());

    let bot = teloxide::Bot::new(&config.telegram_token).throttle(Limits::default());

    // Probe listener for Kubernetes liveness/readiness (see src/health.rs).
    // Bound here rather than in the spawned task so a bind failure is a
    // loud startup error instead of a silently absent probe endpoint.
    let probe_listener = health::bind().await?;
    tokio::spawn(health::serve(probe_listener, state.health.clone()));

    // Registers the command list with Telegram itself, so the client's "/"
    // menu autocompletes every command with its description instead of the
    // user having to remember them or run /help. Best-effort: a failure
    // here (e.g. a transient Telegram API hiccup) shouldn't block startup.
    {
        use teloxide::utils::command::BotCommands;
        if let Err(e) = bot.set_my_commands(bot::Command::bot_commands()).await {
            tracing::warn!(error = %e, "failed to register bot commands with Telegram");
        }
    }

    // Resume watching every account that survived a restart, without
    // re-notifying about anything already seen (last_state is resumed from
    // the encrypted store).
    //
    // spawn_boot_resume registers every persisted target in the health
    // registry (disconnected - /readyz says 503 until they come up) and
    // retries each connect in its own background task, so neither a JMAP
    // server that is still down nor N accounts × a 10s connect timeout may
    // hold the dispatcher below hostage: the bot must answer /login and
    // /logout while it is still bringing its own watchers back.
    for (chat_id, account) in store.all() {
        accounts::spawn_boot_resume(&bot, &state, chat_id, &account);
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
