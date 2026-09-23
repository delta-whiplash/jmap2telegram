use std::collections::HashMap;
use std::sync::Arc;

use jmap_client::client::Client as JmapClient;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::store::Store;

/// (chat id, JMAP account id): identifies one of a chat's opted-in shared
/// accounts, distinct from its own mailbox (which only needs the chat id).
type SharedAccountKey = (i64, String);

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
