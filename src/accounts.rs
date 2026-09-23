//! Wraps an already-connected JMAP `Client` into the shared cache and
//! spawns its watcher — the common tail of every "a chat just connected
//! (or resumed) an account" path: boot resume, `/login`, `/comptes`, and
//! `/partages`' enable toggle all end here. Kept out of `state::AppState`
//! itself so that module stays a plain cache/lookup container rather than
//! also knowing how to spawn tokio tasks.

use std::sync::Arc;

use jmap_client::client::Client as JmapClient;

use crate::Bot;
use crate::state::AppState;
use crate::watcher::{self, WatchTarget};

/// Adopts a freshly connected client for a chat's own mailbox.
pub async fn adopt_primary(bot: &Bot, state: &AppState, chat_id: i64, client: JmapClient) {
    let client = Arc::new(client);
    state.clients.write().await.insert(chat_id, client.clone());
    let handle = watcher::spawn(
        bot.clone(),
        state.clone(),
        chat_id,
        client,
        WatchTarget::Primary,
    );
    state.watchers.write().await.insert(chat_id, handle);
}

/// Adopts a freshly connected client for one of a chat's shared/delegated
/// JMAP accounts.
pub async fn adopt_shared(
    bot: &Bot,
    state: &AppState,
    chat_id: i64,
    account_id: String,
    label: String,
    client: JmapClient,
) {
    let client = Arc::new(client);
    let key = (chat_id, account_id.clone());
    state
        .shared_clients
        .write()
        .await
        .insert(key.clone(), client.clone());
    let handle = watcher::spawn(
        bot.clone(),
        state.clone(),
        chat_id,
        client,
        WatchTarget::Shared { account_id, label },
    );
    state.shared_watchers.write().await.insert(key, handle);
}

/// Adopts a freshly connected client for one of a chat's extra, fully
/// independent personal accounts.
pub async fn adopt_extra(
    bot: &Bot,
    state: &AppState,
    chat_id: i64,
    slot_id: String,
    label: String,
    client: JmapClient,
) {
    let client = Arc::new(client);
    let key = (chat_id, slot_id.clone());
    state
        .extra_clients
        .write()
        .await
        .insert(key.clone(), client.clone());
    let handle = watcher::spawn(
        bot.clone(),
        state.clone(),
        chat_id,
        client,
        WatchTarget::Extra { slot_id, label },
    );
    state.extra_watchers.write().await.insert(key, handle);
}
