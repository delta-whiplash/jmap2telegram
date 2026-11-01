use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use jmap_client::client::Client as JmapClient;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::store::Store;

/// (chat id, slot id): identifies one of a chat's connected JMAP accounts.
pub type AccountKey = (i64, String);

/// (chat id, JMAP account id): identifies one of a chat's opted-in shared accounts.
pub type SharedAccountKey = (i64, String);

/// Backward-compatible alias for `AccountKey`.
#[allow(dead_code)]
pub type ExtraAccountKey = AccountKey;

/// How long `/undo` (the "↩️ Annuler" button left after a triage action)
/// stays valid. Deliberately short and in-memory only: this is a "catch an
/// accidental tap" safety net, not a durable trash - a bot restart or the
/// window elapsing just means the action stands, same as if undo never
/// existed.
pub const UNDO_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

/// A pending undo for the last triage action (archive/spam/delete) taken
/// on a chat's notification, keyed by chat id - one slot per chat, so a
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
    /// Live watcher registry backing /metrics (see health.rs).
    pub health: Arc<crate::health::Health>,
    /// Application counters/gauges rendered by /metrics (see health.rs).
    pub metrics: Arc<crate::health::Metrics>,
    /// Live JMAP clients for connected personal accounts, keyed by (chat_id, slot_id).
    pub account_clients: Arc<RwLock<HashMap<AccountKey, Arc<JmapClient>>>>,
    pub account_watchers: Arc<RwLock<HashMap<AccountKey, JoinHandle<()>>>>,
    /// Same as `account_clients`/`account_watchers` but for opted-in shared JMAP accounts.
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
            health: Arc::new(crate::health::Health::default()),
            metrics: Arc::new(crate::health::Metrics::new()),
            account_clients: Arc::new(RwLock::new(HashMap::new())),
            account_watchers: Arc::new(RwLock::new(HashMap::new())),
            shared_clients: Arc::new(RwLock::new(HashMap::new())),
            shared_watchers: Arc::new(RwLock::new(HashMap::new())),
            pending_undo: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Recomputes the `pending_undos` gauge from the source of truth.
    /// Called after every mutation of `pending_undo` so the gauge can never
    /// drift from the map it reports on.
    pub async fn refresh_undo_gauge(&self) {
        self.metrics
            .set_pending_undos(self.pending_undo.read().await.len() as u64);
    }

    /// Recomputes the `accounts` gauge from the store (the number of chats
    /// with a connected JMAP account). Called after every login/logout.
    pub fn refresh_accounts_gauge(&self) {
        self.metrics.set_accounts(self.store.count() as u64);
    }

    /// The locale to render this chat's user-facing text in: the stored
    /// preference (`/lang`, or the `language_code` seeded at login), then
    /// the current message's Telegram `language_code`, then the operator's
    /// `BOT_LANGUAGE` (see [`crate::i18n::resolve`]).
    pub fn language_for(&self, chat_id: i64, telegram_language_code: Option<&str>) -> &'static str {
        let stored = self.store.get(chat_id).and_then(|account| account.language);
        crate::i18n::resolve(
            stored.as_deref(),
            telegram_language_code,
            &self.config.default_language,
        )
    }

    /// Shared cache-lookup step for every `client_for*` method below.
    async fn cached<K: Eq + std::hash::Hash>(
        cache: &RwLock<HashMap<K, Arc<JmapClient>>>,
        key: &K,
    ) -> Option<Arc<JmapClient>> {
        cache.read().await.get(key).cloned()
    }

    /// Shared connect-and-cache tail: wraps a freshly connected `Client`,
    /// stores it under `key` for next time, and returns it.
    async fn cache_new<K: Eq + std::hash::Hash>(
        cache: &RwLock<HashMap<K, Arc<JmapClient>>>,
        key: K,
        client: JmapClient,
    ) -> Arc<JmapClient> {
        let client = Arc::new(client);
        cache.write().await.insert(key, client.clone());
        client
    }

    /// Returns a live client for a specific account slot.
    pub async fn client_for_account(
        &self,
        chat_id: i64,
        slot_id: &str,
    ) -> anyhow::Result<Option<Arc<JmapClient>>> {
        let key = (chat_id, slot_id.to_string());
        if let Some(client) = Self::cached(&self.account_clients, &key).await {
            return Ok(Some(client));
        }

        let Some(account) = self.store.get(chat_id) else {
            return Ok(None);
        };
        let Some(slot) = account.accounts.get(slot_id) else {
            return Ok(None);
        };

        let client = crate::jmap::connect(
            &slot.server_url,
            &slot.token,
            self.config.allow_private_jmap_hosts,
        )
        .await?;
        Ok(Some(
            Self::cache_new(&self.account_clients, key, client).await,
        ))
    }

    /// Returns a live client for this chat's first/default mailbox.
    pub async fn client_for(&self, chat_id: i64) -> anyhow::Result<Option<Arc<JmapClient>>> {
        let Some(account) = self.store.get(chat_id) else {
            return Ok(None);
        };
        let Some(slot) = account.primary_slot() else {
            return Ok(None);
        };
        self.client_for_account(chat_id, &slot.slot_id).await
    }

    /// Returns all live clients for all connected accounts of this chat.
    pub async fn all_clients_for_chat(
        &self,
        chat_id: i64,
    ) -> anyhow::Result<Vec<(crate::store::AccountSlot, Arc<JmapClient>)>> {
        let Some(account) = self.store.get(chat_id) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for (slot_id, slot) in &account.accounts {
            if let Some(client) = self.client_for_account(chat_id, slot_id).await? {
                out.push((slot.clone(), client));
            }
        }
        Ok(out)
    }

    /// Same as `client_for_account`, kept for backward compatibility.
    #[allow(dead_code)]
    pub async fn client_for_extra(
        &self,
        chat_id: i64,
        slot_id: &str,
    ) -> anyhow::Result<Option<Arc<JmapClient>>> {
        self.client_for_account(chat_id, slot_id).await
    }

    /// Same as `client_for`, but for one of the chat's opted-in shared JMAP accounts.
    pub async fn client_for_shared(
        &self,
        chat_id: i64,
        account_id: &str,
    ) -> anyhow::Result<Option<Arc<JmapClient>>> {
        let key = (chat_id, account_id.to_string());
        if let Some(client) = Self::cached(&self.shared_clients, &key).await {
            return Ok(Some(client));
        }

        let Some(account) = self.store.get(chat_id) else {
            return Ok(None);
        };
        let Some(shared) = account.shared_accounts.get(account_id) else {
            return Ok(None);
        };

        let Some(slot) = account.resolve_parent_slot(shared) else {
            return Ok(None);
        };

        let client = crate::jmap::connect_shared(
            &slot.server_url,
            &slot.token,
            self.config.allow_private_jmap_hosts,
            account_id,
        )
        .await?;
        Ok(Some(
            Self::cache_new(&self.shared_clients, key, client).await,
        ))
    }

    /// Tears down everything for a chat: all account watchers and clients,
    /// and all shared-account watchers and clients.
    pub async fn forget(&self, chat_id: i64) {
        self.pending_undo.write().await.remove(&chat_id);
        self.refresh_undo_gauge().await;

        self.health.deregister_chat(chat_id);

        Self::forget_all_for_chat(&self.account_clients, &self.account_watchers, chat_id).await;
        Self::forget_all_for_chat(&self.shared_clients, &self.shared_watchers, chat_id).await;
    }

    /// Drops every cached client and aborts every watcher belonging to `chat_id`.
    async fn forget_all_for_chat<K>(
        clients: &RwLock<HashMap<(i64, K), Arc<JmapClient>>>,
        watchers: &RwLock<HashMap<(i64, K), JoinHandle<()>>>,
        chat_id: i64,
    ) where
        K: Eq + std::hash::Hash,
    {
        clients.write().await.retain(|(id, _), _| *id != chat_id);
        watchers.write().await.retain(|(id, _), handle| {
            if *id == chat_id {
                handle.abort();
                false
            } else {
                true
            }
        });
    }

    /// Tears down just one account's watcher/client, as well as any child shared accounts.
    pub async fn forget_account(&self, chat_id: i64, slot_id: &str) {
        self.health
            .deregister(&crate::watcher::account_health_key(chat_id, slot_id));
        let key = (chat_id, slot_id.to_string());
        self.account_clients.write().await.remove(&key);
        if let Some(handle) = self.account_watchers.write().await.remove(&key) {
            handle.abort();
        }

        // Also tear down any child shared accounts that were authorized under this slot
        if let Some(account) = self.store.get(chat_id) {
            for (account_id, shared) in &account.shared_accounts {
                if shared.is_child_of(slot_id) {
                    self.forget_shared(chat_id, account_id).await;
                }
            }
        }
    }

    /// Backward-compatible alias for `forget_account`.
    #[allow(dead_code)]
    pub async fn forget_extra(&self, chat_id: i64, slot_id: &str) {
        self.forget_account(chat_id, slot_id).await;
    }

    /// Tears down just one shared account's watcher/client.
    pub async fn forget_shared(&self, chat_id: i64, account_id: &str) {
        self.health
            .deregister(&crate::watcher::shared_health_key(chat_id, account_id));
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
    use crate::config::Config;
    use crate::watcher::{self, WatchTarget};

    /// An `AppState` pair around a throwaway encrypted store. The Config
    /// here is never exercised for anything the tests below touch (no JMAP
    /// connects, no Telegram requests) - it just has to exist.
    async fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(Config::for_testing(dir.path(), 42));
        let store = Arc::new(Store::open(dir.path()).unwrap());
        (AppState::new(config, store), dir)
    }

    /// A watcher stand-in that never finishes, so the `forget*` teardowns
    /// have a real `JoinHandle` to remove and abort.
    fn parked_handle() -> JoinHandle<()> {
        tokio::spawn(std::future::pending::<()>())
    }

    /// Registers - and marks connected - the health entries a fully online
    /// chat 42 would hold: its primary mailbox plus one shared and one
    /// extra account, keyed exactly as `watcher::spawn` derives them.
    fn register_chat_42_targets(state: &AppState) {
        for key in [
            WatchTarget::Primary.health_key(42),
            watcher::shared_health_key(42, "acc7"),
            watcher::extra_health_key(42, "slot1"),
        ] {
            state.health.register(&key);
            state.health.set_connected(&key, true);
        }
    }

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

    #[tokio::test]
    async fn forget_deregisters_every_health_entry_of_the_chat() {
        // Regression test for the ghost-entry leak: `handle.abort()` used
        // to leave the registry entry behind, leaking it in the health
        // registry forever for a chat nobody was watching anymore.
        let (state, _dir) = test_state().await;
        register_chat_42_targets(&state);
        // A different chat's watcher must survive chat 42's teardown.
        state.health.register(&WatchTarget::Primary.health_key(43));

        // Live handles, as running watchers would have left behind.
        state
            .account_watchers
            .write()
            .await
            .insert((42, "default".to_string()), parked_handle());
        state
            .shared_watchers
            .write()
            .await
            .insert((42, "acc7".to_string()), parked_handle());
        state
            .account_watchers
            .write()
            .await
            .insert((42, "slot1".to_string()), parked_handle());

        state.forget(42).await;

        let snapshot = state.health.snapshot();
        assert!(!snapshot.contains_key("42/primary"), "{snapshot:?}");
        assert!(!snapshot.contains_key("42/shared-acc7"), "{snapshot:?}");
        assert!(!snapshot.contains_key("42/extra-slot1"), "{snapshot:?}");
        assert!(snapshot.contains_key("43/primary"), "{snapshot:?}");
        // The watcher maps went with it, so a later forget is a no-op
        // rather than an abort of a stale handle.
        assert!(state.account_watchers.read().await.is_empty());
        assert!(state.shared_watchers.read().await.is_empty());
    }

    #[tokio::test]
    async fn forget_clears_entries_of_targets_without_a_running_watcher() {
        // Boot-resume shape (see accounts.rs): the registry entry exists -
        // registered *before* the first connect attempt - but no handle
        // sits in the watcher maps yet. A /logout racing that retry must
        // still clear the entry, or the registry would leak it forever.
        let (state, _dir) = test_state().await;
        state
            .health
            .register(&watcher::shared_health_key(42, "acc7"));
        state
            .health
            .register(&watcher::extra_health_key(42, "slot1"));

        state.forget(42).await;

        assert!(state.health.snapshot().is_empty());
    }

    #[tokio::test]
    async fn forget_shared_deregisters_only_that_accounts_entry() {
        let (state, _dir) = test_state().await;
        register_chat_42_targets(&state);
        // A second shared account of the same chat: it has an entry but no
        // handle (boot-resume shape), and toggling acc7 off must not touch
        // it.
        state
            .health
            .register(&watcher::shared_health_key(42, "acc8"));
        state
            .shared_watchers
            .write()
            .await
            .insert((42, "acc7".to_string()), parked_handle());

        state.forget_shared(42, "acc7").await;

        let snapshot = state.health.snapshot();
        assert!(!snapshot.contains_key("42/shared-acc7"), "{snapshot:?}");
        assert!(snapshot.contains_key("42/primary"), "{snapshot:?}");
        assert!(snapshot.contains_key("42/extra-slot1"), "{snapshot:?}");
        assert!(snapshot.contains_key("42/shared-acc8"), "{snapshot:?}");
        // The snapshot reflects the surviving state exactly: acc8 was
        // registered but never connected, all others are connected.
        let acc8_key = watcher::shared_health_key(42, "acc8");
        assert!(snapshot.contains_key(&acc8_key), "{snapshot:?}");
        assert!(!snapshot.get(&acc8_key).unwrap().connected);
        assert!(snapshot.values().filter(|s| s.connected).count() == snapshot.len() - 1);
        state.health.set_connected(&acc8_key, true);
        let snapshot = state.health.snapshot();
        assert!(snapshot.get(&acc8_key).unwrap().connected);
    }

    #[tokio::test]
    async fn forget_extra_deregisters_only_that_slots_entry() {
        let (state, _dir) = test_state().await;
        register_chat_42_targets(&state);
        // No handle for slot1 here on purpose: the deregister must not
        // depend on there being a watcher to abort.
        state
            .account_watchers
            .write()
            .await
            .insert((42, "slot1".to_string()), parked_handle());

        state.forget_extra(42, "slot1").await;

        let snapshot = state.health.snapshot();
        assert!(!snapshot.contains_key("42/extra-slot1"), "{snapshot:?}");
        assert!(snapshot.contains_key("42/primary"), "{snapshot:?}");
        assert!(snapshot.contains_key("42/shared-acc7"), "{snapshot:?}");
    }

    #[tokio::test]
    async fn forget_default_account_cleans_up_shared_watchers_with_none_parent_slot() {
        let (state, _dir) = test_state().await;
        let mut acc = crate::store::Account::new(
            "https://127.0.0.1:9".to_string(),
            "tok".to_string(),
            "user@example.org".to_string(),
        );
        acc.shared_accounts.insert(
            "acc7".to_string(),
            crate::store::SharedAccount {
                name: "shared@example.org".to_string(),
                last_state: None,
                parent_slot: None,
                thread_id: None,
            },
        );
        state.store.set(42, acc).unwrap();

        state
            .account_watchers
            .write()
            .await
            .insert((42, "default".to_string()), parked_handle());
        state
            .shared_watchers
            .write()
            .await
            .insert((42, "acc7".to_string()), parked_handle());

        state.forget_account(42, "default").await;

        assert!(
            state.account_watchers.read().await.is_empty(),
            "default account watcher should be removed"
        );
        assert!(
            state.shared_watchers.read().await.is_empty(),
            "child shared watcher should be removed when default account is forgotten"
        );
    }
}
