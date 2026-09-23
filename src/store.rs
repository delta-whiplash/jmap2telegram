use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Context, Result, bail};
use rand::{Rng, rng};
use serde::{Deserialize, Serialize};

const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// A single authorized user's JMAP account, as entered live through the bot.
///
/// This is the only personal data the bot ever persists. No email content
/// is ever written to disk: `last_state` is just an opaque JMAP sync
/// cursor, not a copy of any message.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Account {
    pub server_url: String,
    pub token: String,
    pub email: String,
    pub last_state: Option<String>,
    /// JMAP delegated/shared accounts (e.g. a shared team mailbox) the user
    /// has opted into notifications for, keyed by JMAP account id.
    /// `#[serde(default)]` keeps this backward compatible with state files
    /// written before this field existed.
    #[serde(default)]
    pub shared_accounts: HashMap<String, SharedAccount>,
}

/// A shared JMAP account the user has opted into. Presence in
/// `Account::shared_accounts` *is* the opt-in: there is no separate
/// enabled flag to fall out of sync with the watcher/client state.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SharedAccount {
    /// Display name from the JMAP session (e.g. the shared mailbox's
    /// address), shown in notifications and the toggle menu.
    pub name: String,
    pub last_state: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct StoreData {
    /// Telegram chat id -> connected JMAP account.
    accounts: HashMap<i64, Account>,
}

/// Encrypted-at-rest, on-disk key/value store for per-chat JMAP accounts.
///
/// The state file (`state.enc`) is AES-256-GCM encrypted with a random key
/// generated on first boot and kept in `master.key` next to it, both under
/// 0600 permissions. This protects credentials against casual disk/backup
/// leakage; anyone with access to the running container's filesystem still
/// has access to both files, which is the accepted trade-off for a
/// deployment with no third env var / external KMS.
pub struct Store {
    state_path: PathBuf,
    key: [u8; KEY_LEN],
    data: RwLock<StoreData>,
    /// Serializes `persist()` calls. Several tokio tasks (one watcher per
    /// connected chat, plus message handlers) can call `set`/`update_state`/
    /// `remove` concurrently; without this, two `persist()` calls could
    /// interleave their create/write/chmod/rename on the *same* fixed tmp
    /// path, so one call's rename can vanish from under the other's,
    /// surfacing a spurious I/O error (or worse, a torn file) even though
    /// the in-memory state was updated correctly.
    persist_lock: Mutex<()>,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating data dir {dir:?}"))?;

        let key_path = dir.join("master.key");
        let key = load_or_create_key(&key_path)?;

        let state_path = dir.join("state.enc");
        let data = if state_path.exists() {
            let ciphertext = fs::read(&state_path).context("reading state file")?;
            decrypt(&key, &ciphertext)
                .context("decrypting state file (master key changed or corrupted state?)")?
        } else {
            StoreData::default()
        };

        Ok(Self {
            state_path,
            key,
            data: RwLock::new(data),
            persist_lock: Mutex::new(()),
        })
    }

    pub fn get(&self, chat_id: i64) -> Option<Account> {
        self.data.read().unwrap().accounts.get(&chat_id).cloned()
    }

    pub fn all(&self) -> Vec<(i64, Account)> {
        self.data
            .read()
            .unwrap()
            .accounts
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    pub fn set(&self, chat_id: i64, account: Account) -> Result<()> {
        {
            let mut data = self.data.write().unwrap();
            data.accounts.insert(chat_id, account);
        }
        self.persist()
    }

    pub fn update_state(&self, chat_id: i64, state: String) -> Result<()> {
        {
            let mut data = self.data.write().unwrap();
            match data.accounts.get_mut(&chat_id) {
                Some(acc) => acc.last_state = Some(state),
                None => return Ok(()),
            }
        }
        self.persist()
    }

    /// Opts a chat into notifications for a shared JMAP account, or updates
    /// its display name/cursor if already opted in. No-ops (without error)
    /// if the chat has no primary account, since a shared account can't
    /// outlive the login it was discovered through.
    pub fn set_shared_account(
        &self,
        chat_id: i64,
        account_id: String,
        name: String,
        last_state: Option<String>,
    ) -> Result<()> {
        {
            let mut data = self.data.write().unwrap();
            match data.accounts.get_mut(&chat_id) {
                Some(acc) => {
                    acc.shared_accounts
                        .insert(account_id, SharedAccount { name, last_state });
                }
                None => return Ok(()),
            }
        }
        self.persist()
    }

    /// Opts a chat out of notifications for a shared JMAP account. Returns
    /// whether it was actually opted in.
    pub fn remove_shared_account(&self, chat_id: i64, account_id: &str) -> Result<bool> {
        let removed = {
            let mut data = self.data.write().unwrap();
            match data.accounts.get_mut(&chat_id) {
                Some(acc) => acc.shared_accounts.remove(account_id).is_some(),
                None => false,
            }
        };
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    pub fn update_shared_account_state(
        &self,
        chat_id: i64,
        account_id: &str,
        state: String,
    ) -> Result<()> {
        {
            let mut data = self.data.write().unwrap();
            match data
                .accounts
                .get_mut(&chat_id)
                .and_then(|acc| acc.shared_accounts.get_mut(account_id))
            {
                Some(shared) => shared.last_state = Some(state),
                None => return Ok(()),
            }
        }
        self.persist()
    }

    /// Removes a chat's account. This is the GDPR "right to erasure" path:
    /// once this returns, nothing about that user remains on disk.
    pub fn remove(&self, chat_id: i64) -> Result<bool> {
        let removed = {
            let mut data = self.data.write().unwrap();
            data.accounts.remove(&chat_id).is_some()
        };
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    fn persist(&self) -> Result<()> {
        // Serializes the whole read-encrypt-write-rename sequence,
        // including the snapshot of `data`: several tokio tasks (one
        // watcher per connected chat, plus message handlers) can call
        // `set`/`update_state`/`remove` concurrently, and without this
        // lock two persist() calls could interleave their file I/O on the
        // same fixed tmp path (one's rename disappearing from under the
        // other), or a slower call could overwrite a newer one's file
        // with a stale snapshot taken before the lock.
        let _guard = self.persist_lock.lock().unwrap();

        let plaintext = {
            let data = self.data.read().unwrap();
            serde_json::to_vec(&*data)?
        };
        let ciphertext = encrypt(&self.key, &plaintext)?;

        // Write-then-rename for crash safety; never leave a half-written
        // state file in place of a good one.
        let tmp_path = self.state_path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp_path)?;
            f.write_all(&ciphertext)?;
            f.sync_all()?;
        }
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp_path, &self.state_path)?;
        Ok(())
    }
}

fn load_or_create_key(key_path: &Path) -> Result<[u8; KEY_LEN]> {
    if key_path.exists() {
        let bytes = fs::read(key_path).context("reading master key")?;
        if bytes.len() != KEY_LEN {
            bail!(
                "master key file has unexpected length ({} bytes)",
                bytes.len()
            );
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&bytes);
        Ok(key)
    } else {
        let mut key = [0u8; KEY_LEN];
        rng().fill_bytes(&mut key);
        fs::write(key_path, key).context("writing master key")?;
        fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))?;
        Ok(key)
    }
}

fn encrypt(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);

    let mut ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;

    let mut out = nonce_bytes.to_vec();
    out.append(&mut ciphertext);
    Ok(out)
}

fn decrypt(key: &[u8; KEY_LEN], data: &[u8]) -> Result<StoreData> {
    if data.len() < NONCE_LEN {
        bail!("state file too short");
    }
    let (nonce_bytes, ciphertext) = data.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key));
    let nonce_arr: [u8; NONCE_LEN] = nonce_bytes.try_into().expect("checked length above");
    let nonce = Nonce::from(nonce_arr);

    let plaintext = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("decryption failed: {e}"))?;

    Ok(serde_json::from_slice(&plaintext)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(email: &str) -> Account {
        Account {
            server_url: "https://jmap.example.org/session".to_string(),
            token: "s3cr3t-token".to_string(),
            email: email.to_string(),
            last_state: None,
            shared_accounts: HashMap::new(),
        }
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [7u8; KEY_LEN];
        let plaintext = b"hello world, this is a secret".to_vec();
        let ciphertext = encrypt(&key, &plaintext).unwrap();
        assert_ne!(ciphertext, plaintext, "ciphertext must not equal plaintext");

        let mut data = StoreData::default();
        data.accounts.insert(1, account("a@b.c"));
        let plaintext = serde_json::to_vec(&data).unwrap();
        let ciphertext = encrypt(&key, &plaintext).unwrap();
        let decrypted = decrypt(&key, &ciphertext).unwrap();
        assert_eq!(decrypted.accounts.len(), 1);
        assert_eq!(decrypted.accounts[&1].email, "a@b.c");
    }

    #[test]
    fn decrypt_fails_with_wrong_key() {
        let key_a = [1u8; KEY_LEN];
        let key_b = [2u8; KEY_LEN];
        let ciphertext = encrypt(&key_a, b"top secret").unwrap();
        assert!(decrypt(&key_b, &ciphertext).is_err());
    }

    #[test]
    fn decrypt_fails_on_truncated_data() {
        let key = [3u8; KEY_LEN];
        assert!(decrypt(&key, b"short").is_err());
    }

    #[test]
    fn two_encryptions_of_same_plaintext_use_different_nonces() {
        let key = [9u8; KEY_LEN];
        let a = encrypt(&key, b"same plaintext").unwrap();
        let b = encrypt(&key, b"same plaintext").unwrap();
        assert_ne!(a, b, "nonce reuse would leak that plaintexts are identical");
    }

    #[test]
    fn set_get_and_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.set(42, account("user@example.org")).unwrap();
        assert_eq!(store.get(42).unwrap().email, "user@example.org");

        // Reopen: must survive a restart using the same on-disk key/state.
        drop(store);
        let reopened = Store::open(dir.path()).unwrap();
        assert_eq!(reopened.get(42).unwrap().email, "user@example.org");
    }

    #[test]
    fn master_key_file_has_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let _store = Store::open(dir.path()).unwrap();
        let meta = fs::metadata(dir.path().join("master.key")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn remove_erases_account_and_reports_whether_it_existed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.set(1, account("a@b.c")).unwrap();

        assert!(store.remove(1).unwrap());
        assert!(store.get(1).is_none());
        // Erasure must be durable, not just in-memory.
        let reopened = Store::open(dir.path()).unwrap();
        assert!(reopened.get(1).is_none());

        // Removing again is a no-op, not an error.
        assert!(!store.remove(1).unwrap());
    }

    #[test]
    fn update_state_is_ignored_for_unknown_chat() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        // Must not panic or create a phantom account.
        store.update_state(999, "state-x".to_string()).unwrap();
        assert!(store.get(999).is_none());
    }

    #[test]
    fn set_shared_account_adds_and_updates_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.set(1, account("a@b.c")).unwrap();

        store
            .set_shared_account(1, "acc7".to_string(), "shared@b.c".to_string(), None)
            .unwrap();
        let shared = &store.get(1).unwrap().shared_accounts["acc7"];
        assert_eq!(shared.name, "shared@b.c");
        assert_eq!(shared.last_state, None);

        // Re-setting the same id updates it in place rather than erroring.
        store
            .set_shared_account(
                1,
                "acc7".to_string(),
                "shared@b.c".to_string(),
                Some("state-1".to_string()),
            )
            .unwrap();
        assert_eq!(
            store.get(1).unwrap().shared_accounts["acc7"].last_state,
            Some("state-1".to_string())
        );
    }

    #[test]
    fn set_shared_account_is_ignored_for_unknown_chat() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .set_shared_account(999, "acc1".to_string(), "x@y.z".to_string(), None)
            .unwrap();
        assert!(store.get(999).is_none());
    }

    #[test]
    fn remove_shared_account_erases_entry_and_reports_whether_it_existed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.set(1, account("a@b.c")).unwrap();
        store
            .set_shared_account(1, "acc7".to_string(), "shared@b.c".to_string(), None)
            .unwrap();

        assert!(store.remove_shared_account(1, "acc7").unwrap());
        assert!(!store.get(1).unwrap().shared_accounts.contains_key("acc7"));
        assert!(!store.remove_shared_account(1, "acc7").unwrap());
    }

    #[test]
    fn update_shared_account_state_is_ignored_for_unknown_account() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.set(1, account("a@b.c")).unwrap();
        // Must not panic or create a phantom shared account.
        store
            .update_shared_account_state(1, "does-not-exist", "state-x".to_string())
            .unwrap();
        assert!(store.get(1).unwrap().shared_accounts.is_empty());
    }

    /// Backward compatibility: state files written before `shared_accounts`
    /// existed must still decrypt and parse, defaulting to an empty map,
    /// rather than failing every deployment upgrading in place.
    #[test]
    fn account_without_shared_accounts_field_deserializes_with_empty_map() {
        let json = r#"{"server_url":"https://jmap.example.org","token":"t","email":"a@b.c","last_state":null}"#;
        let acc: Account = serde_json::from_str(json).unwrap();
        assert!(acc.shared_accounts.is_empty());
    }

    #[test]
    fn all_lists_every_stored_account() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.set(1, account("a@b.c")).unwrap();
        store.set(2, account("d@e.f")).unwrap();
        let mut all = store.all();
        all.sort_by_key(|(id, _)| *id);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, 1);
        assert_eq!(all[1].0, 2);
    }
}
