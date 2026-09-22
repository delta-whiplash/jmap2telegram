use std::collections::HashMap;
use std::sync::Arc;

use jmap_client::client::Client as JmapClient;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::store::Store;

/// Shared application state, injected into every Telegram handler and into
/// the background per-account JMAP watchers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Arc<Store>,
    /// Live JMAP clients, keyed by chat id, shared between the watcher
    /// tasks and the inline-button handlers so an action doesn't need a
    /// fresh session negotiation every time.
    pub clients: Arc<RwLock<HashMap<i64, Arc<JmapClient>>>>,
    pub watchers: Arc<RwLock<HashMap<i64, JoinHandle<()>>>>,
}

impl AppState {
    pub fn new(config: Arc<Config>, store: Arc<Store>) -> Self {
        Self {
            config,
            store,
            clients: Arc::new(RwLock::new(HashMap::new())),
            watchers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Returns a live client for this chat, reconnecting from the stored
    /// credentials if none is cached yet (e.g. after a fresh reconnect
    /// following a bot restart, before the watcher has re-established it).
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

    pub async fn forget(&self, chat_id: i64) {
        self.clients.write().await.remove(&chat_id);
        if let Some(handle) = self.watchers.write().await.remove(&chat_id) {
            handle.abort();
        }
    }
}
