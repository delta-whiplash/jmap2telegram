use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use jmap_client::client::Client as JmapClient;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::store::Store;

/// (chat id, JMAP account id): identifies one of a chat's opted-in shared
/// accounts, distinct from its own mailbox (which only needs the chat id).
type SharedAccountKey = (i64, String);

/// How long `/undo` (the "↩️ Annuler" button left after a triage action)
/// stays valid. Deliberately short and in-memory only: this is a "catch an
/// accidental tap" safety net, not a durable trash — a bot restart or the
/// window elapsing just means the action stands, same as if undo never
/// existed.
pub const UNDO_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

/// A pending undo for the last triage action (archive/spam/delete) taken
/// on a chat's notification, keyed by chat id — one slot per chat, so a
/// second action before the first is undone simply replaces it, matching
/// "undo the last thing" rather than a full history.
#[derive(Clone)]
pub struct PendingUndo {
    pub account_id: Option<String>,
    pub email_id: String,
    /// The mailbox ids the message was in right before the action, so
    /// undo restores exactly that rather than guessing "back to Inbox".
    pub restore_mailbox_ids: Vec<String>,
    pub expires_at: Instant,
}

impl PendingUndo {
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// Shared application state, injected into every Telegram handler and into
/// the background per-account JMAP watchers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Arc<Store>,
    /// Live JMAP clients for each chat's own mailbox, keyed by chat id,
    /// shared between the watcher tasks and the inline-button handlers so
    /// an action doesn't need a fresh session negotiation every time.
    pub clients: Arc<RwLock<HashMap<i64, Arc<JmapClient>>>>,
    pub watchers: Arc<RwLock<HashMap<i64, JoinHandle<()>>>>,
    /// Same as `clients`/`watchers` but for opted-in shared JMAP accounts.
    /// These need their own `Client` per account: `jmap-client` tracks one
    /// default account per `Client`, and mutating a shared `Arc<Client>`'s
    /// default account out from under concurrent tasks would race.
    pub shared_clients: Arc<RwLock<HashMap<SharedAccountKey, Arc<JmapClient>>>>,
    pub shared_watchers: Arc<RwLock<HashMap<SharedAccountKey, JoinHandle<()>>>>,
    /// One pending `/undo` slot per chat. In-memory only, never persisted:
    /// this is a short-lived safety net for an accidental tap, not
    /// durable state anything depends on surviving a restart.
    pub pending_undo: Arc<RwLock<HashMap<i64, PendingUndo>>>,
}

impl AppState {
    pub fn new(config: Arc<Config>, store: Arc<Store>) -> Self {
        Self {
            config,
            store,
            clients: Arc::new(RwLock::new(HashMap::new())),
            watchers: Arc::new(RwLock::new(HashMap::new())),
            shared_clients: Arc::new(RwLock::new(HashMap::new())),
            shared_watchers: Arc::new(RwLock::new(HashMap::new())),
            pending_undo: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Returns a live client for this chat's own mailbox, reconnecting from
    /// the stored credentials if none is cached yet (e.g. after a fresh
    /// reconnect following a bot restart, before the watcher has
    /// re-established it).
    pub async fn client_for(&self, chat_id: i64) -> anyhow::Result<Option<Arc<JmapClient>>> {
        if let Some(client) = self.clients.read().await.get(&chat_id) {
            return Ok(Some(client.clone()));
        }

        let Some(account) = self.store.get(chat_id) else {
            return Ok(None);
        };

        let client = Arc::new(
            crate::jmap::connect(
                &account.server_url,
                &account.token,
                self.config.allow_private_jmap_hosts,
            )
            .await?,
        );
        self.clients.write().await.insert(chat_id, client.clone());
        Ok(Some(client))
    }

    /// Same as `client_for`, but for one of the chat's opted-in shared JMAP
    /// accounts. Returns `None` both when the chat has no primary account
    /// and when it hasn't (or no longer) opted into this specific shared
    /// account, e.g. a stale inline button tapped after the account was
    /// toggled off.
    pub async fn client_for_shared(
        &self,
        chat_id: i64,
        account_id: &str,
    ) -> anyhow::Result<Option<Arc<JmapClient>>> {
        let key = (chat_id, account_id.to_string());
        if let Some(client) = self.shared_clients.read().await.get(&key) {
            return Ok(Some(client.clone()));
        }

        let Some(account) = self.store.get(chat_id) else {
            return Ok(None);
        };
        if !account.shared_accounts.contains_key(account_id) {
            return Ok(None);
        }

        let client = Arc::new(
            crate::jmap::connect_shared(
                &account.server_url,
                &account.token,
                self.config.allow_private_jmap_hosts,
                account_id,
            )
            .await?,
        );
        self.shared_clients
            .write()
            .await
            .insert(key, client.clone());
        Ok(Some(client))
    }

    /// Tears down everything for a chat: its own mailbox watcher/client and
    /// every shared-account watcher/client. Used on /logout and before
    /// re-establishing a fresh connection on /login, so a stale watcher
    /// never keeps running under an id that's since been reused or removed.
    pub async fn forget(&self, chat_id: i64) {
        self.clients.write().await.remove(&chat_id);
        if let Some(handle) = self.watchers.write().await.remove(&chat_id) {
            handle.abort();
        }
        self.pending_undo.write().await.remove(&chat_id);

        let mut shared_clients = self.shared_clients.write().await;
        shared_clients.retain(|(id, _), _| *id != chat_id);
        drop(shared_clients);

        let mut shared_watchers = self.shared_watchers.write().await;
        shared_watchers.retain(|(id, _), handle| {
            if *id == chat_id {
                handle.abort();
                false
            } else {
                true
            }
        });
    }

    /// Tears down just one shared account's watcher/client, leaving the
    /// chat's own mailbox and its other shared accounts untouched. Used
    /// when a single shared account is toggled off via /partages.
    pub async fn forget_shared(&self, chat_id: i64, account_id: &str) {
        let key = (chat_id, account_id.to_string());
        self.shared_clients.write().await.remove(&key);
        if let Some(handle) = self.shared_watchers.write().await.remove(&key) {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_undo_is_expired_reflects_the_deadline() {
        let fresh = PendingUndo {
            account_id: None,
            email_id: "M1".to_string(),
            restore_mailbox_ids: vec!["inbox".to_string()],
            expires_at: Instant::now() + std::time::Duration::from_secs(30),
        };
        assert!(!fresh.is_expired());

        let stale = PendingUndo {
            expires_at: Instant::now() - std::time::Duration::from_secs(1),
            ..fresh
        };
        assert!(stale.is_expired());
    }
}
