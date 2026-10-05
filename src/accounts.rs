//! Wraps an already-connected JMAP `Client` into the shared cache and
//! spawns its watcher — the common tail of every "a chat just connected
//! (or resumed) an account" path: `/login`, `/comptes`, `/partages`'
//! enable toggle and boot resume all end here. Kept out of
//! `state::AppState` itself so that module stays a plain cache/lookup
//! container rather than also knowing how to spawn tokio tasks.
//!
//! Boot resume is the newest tenant: [`spawn_boot_resume`] registers every
//! persisted target in the health registry *before* any connect attempt,
//! then retries each target's connect in its own background task on the
//! watcher's backoff schedule (see [`crate::watcher::INITIAL_BACKOFF`]) —
//! so a bot restarting against a JMAP server that is still down stays
//! /readyz-honest (the accounts show up as disconnected) while the
//! Telegram dispatcher is already live.

use std::sync::Arc;

use jmap_client::client::Client as JmapClient;

use crate::Bot;
use crate::config::Config;
use crate::jmap;
use crate::state::AppState;
use crate::store::Account;
use crate::watcher::{self, INITIAL_BACKOFF, WatchTarget, next_backoff};

/// Adopts a freshly connected client for a chat's own mailbox.
///
/// Idempotent per chat: returns `false` (dropping the fresh client)
/// rather than spawning a second watcher when this slot already holds
/// one. The callers that need this are the concurrency-introduced ones —
/// a boot-resume retry can land right after a `/login` adopted the same
/// chat — but every caller benefits (a double-tapped inline button can't
/// double-watch either).
pub async fn adopt_primary(bot: &Bot, state: &AppState, chat_id: i64, client: JmapClient) -> bool {
    // Holding the watcher-map write lock across check-and-insert is what
    // makes adoption atomic: two concurrent adopters of the same slot
    // can't both pass the contains_key check and end up with two live
    // watchers (and a leaked first JoinHandle, since insert would
    // silently overwrite it). The client-map lock is only ever taken
    // inside the watcher-map lock (never the reverse), so this nesting
    // can't deadlock.
    let mut watchers = state.watchers.write().await;
    if watchers.contains_key(&chat_id) {
        return false;
    }
    let client = Arc::new(client);
    state.clients.write().await.insert(chat_id, client.clone());
    let handle = watcher::spawn(
        bot.clone(),
        state.clone(),
        chat_id,
        client,
        WatchTarget::Primary,
    );
    watchers.insert(chat_id, handle);
    true
}

/// Adopts a freshly connected client for one of a chat's shared/delegated
/// JMAP accounts. Idempotent per (chat, account) — see
/// [`adopt_primary`].
pub async fn adopt_shared(
    bot: &Bot,
    state: &AppState,
    chat_id: i64,
    account_id: String,
    label: String,
    client: JmapClient,
) -> bool {
    let key = (chat_id, account_id.clone());
    let mut watchers = state.shared_watchers.write().await;
    if watchers.contains_key(&key) {
        return false;
    }
    let client = Arc::new(client);
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
    watchers.insert(key, handle);
    true
}

/// Adopts a freshly connected client for one of a chat's extra, fully
/// independent personal accounts. Idempotent per (chat, slot) — see
/// [`adopt_primary`].
pub async fn adopt_extra(
    bot: &Bot,
    state: &AppState,
    chat_id: i64,
    slot_id: String,
    label: String,
    client: JmapClient,
) -> bool {
    let key = (chat_id, slot_id.clone());
    let mut watchers = state.extra_watchers.write().await;
    if watchers.contains_key(&key) {
        return false;
    }
    let client = Arc::new(client);
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
    watchers.insert(key, handle);
    true
}

/// One persisted target worth resuming at boot: which JMAP account to
/// watch (and how to label and route notifications for it), plus the
/// credentials to connect it with. Shared accounts connect with the
/// chat's primary token (delegated access); extra accounts carry their
/// own server/token, which may point at a different server than the
/// primary.
#[derive(Clone, Debug)]
struct ResumeTarget {
    target: WatchTarget,
    server_url: String,
    token: String,
}

/// The targets boot must bring back online for one persisted account: its
/// primary mailbox, every opted-in shared account and every extra
/// account.
fn persisted_targets(account: &Account) -> Vec<ResumeTarget> {
    let mut targets = vec![ResumeTarget {
        target: WatchTarget::Primary,
        server_url: account.server_url.clone(),
        token: account.token.clone(),
    }];
    for (account_id, shared) in &account.shared_accounts {
        targets.push(ResumeTarget {
            target: WatchTarget::Shared {
                account_id: account_id.clone(),
                label: shared.name.clone(),
            },
            // Delegated access rides on the chat's primary credentials.
            server_url: account.server_url.clone(),
            token: account.token.clone(),
        });
    }
    for (slot_id, extra) in &account.extra_accounts {
        targets.push(ResumeTarget {
            target: WatchTarget::Extra {
                slot_id: slot_id.clone(),
                label: extra.email.clone(),
            },
            server_url: extra.server_url.clone(),
            token: extra.token.clone(),
        });
    }
    targets
}

/// Registers every target's health key with `connected: false`, before
/// any connect attempt is made. That ordering is the whole point: an
/// account whose server is still down at boot shows up in /readyz as a
/// named, disconnected watcher (503) rather than being silently absent
/// from the registry (a green pod watching nothing), and the entry only
/// disappears when a watcher takes the target over (re-registering the
/// same key) or a `forget*` teardown drops it.
///
/// Split from [`spawn_boot_resume`] as its own step so tests can observe
/// the registry exactly as boot leaves it, without a connect happening
/// first.
fn register_persisted_targets(state: &AppState, chat_id: i64, targets: &[ResumeTarget]) {
    for target in targets {
        state.health.register(&target.target.health_key(chat_id));
    }
}

/// Registers every persisted target of one account (see
/// [`register_persisted_targets`]) and spawns one background resume task
/// per target. Called from `main` for each account that survived a
/// restart, and deliberately synchronous: the dispatcher starts right
/// after, so boot resume never blocks the Telegram poll loop — with N
/// accounts each facing a ~10s connect timeout, the previous sequential
/// resume delayed command handling by up to N × 10s on every boot.
pub fn spawn_boot_resume(bot: &Bot, state: &AppState, chat_id: i64, account: &Account) {
    let targets = persisted_targets(account);
    register_persisted_targets(state, chat_id, &targets);
    for target in targets {
        let bot = bot.clone();
        let state = state.clone();
        tokio::spawn(resume_target_with(bot, state, chat_id, target, RealConnect));
    }
}

/// Brings one persisted target back online after a restart: retry the
/// JMAP connect on the watcher's backoff schedule (2s doubling to 300s —
/// the same policy `watcher::spawn` uses to heal a dropped stream, via
/// the shared [`crate::watcher::INITIAL_BACKOFF`]/[`next_backoff`])
/// until it succeeds, then hand the client over to the watcher via the
/// `adopt_*` tail.
///
/// The loop gives up only when the target stopped being worth resuming:
/// the account was removed from the store (/logout raced the retry — the
/// matching `forget*` already deregistered the health entry) or a newer
/// watcher already adopted the slot (a `/login` while we were still
/// retrying pre-restart credentials, a `/partages` re-enable). Anything
/// else — server down, token being rotated, network flapping — is just
/// another round of backoff, which is what makes a persisted account no
/// longer silently dead at boot: either its watcher comes up, or /readyz
/// keeps saying so.
async fn resume_target_with(
    bot: Bot,
    state: AppState,
    chat_id: i64,
    target: ResumeTarget,
    mut connect: impl ConnectJmap,
) {
    let health_key = target.target.health_key(chat_id);
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match state.store.get(chat_id) {
            Some(account) if target.target.still_active(&account) => {}
            _ => {
                // /logout, /partages toggle-off or /comptes disconnect
                // while we were retrying. The corresponding forget*
                // teardown already deregistered the entry; deregistering
                // again is an idempotent no-op that also covers store-only
                // mutations.
                state.health.deregister(&health_key);
                tracing::info!(
                    chat_id,
                    health_key = %health_key,
                    "target opted out during boot resume, giving up"
                );
                return;
            }
        }

        if slot_adopted(&state, chat_id, &target.target).await {
            // Someone else owns this slot now. Their watcher re-registered
            // the health entry under the same key, so the registry stays
            // exact — we just stop instead of pointlessly retrying
            // credentials that were never going to be adopted anyway.
            tracing::info!(
                chat_id,
                health_key = %health_key,
                "target already adopted by a newer watcher, stopping boot resume"
            );
            return;
        }

        match connect.connect(&state.config, &target).await {
            Ok(client) => {
                // Someone may still have adopted this slot while we were
                // connecting (the check above can't cover the connect
                // itself): adopt_*'s idempotency guard is the
                // linearization point, and their watcher now owns the
                // health entry — bowing out without touching it keeps the
                // registry exact.
                if adopt_target(&bot, &state, chat_id, &target.target, client).await {
                    tracing::info!(chat_id, health_key = %health_key, "resumed JMAP watcher");
                }
                return;
            }
            Err(e) => {
                tracing::warn!(
                    chat_id,
                    health_key = %health_key,
                    error = %e,
                    "failed to resume JMAP account, retrying with backoff"
                );
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff);
    }
}

/// Whether some other path (a `/login`, a `/partages` re-enable, an
/// earlier resume attempt) already holds a live watcher for this slot.
async fn slot_adopted(state: &AppState, chat_id: i64, target: &WatchTarget) -> bool {
    match target {
        WatchTarget::Primary => state.watchers.read().await.contains_key(&chat_id),
        WatchTarget::Shared { account_id, .. } => state
            .shared_watchers
            .read()
            .await
            .contains_key(&(chat_id, account_id.clone())),
        WatchTarget::Extra { slot_id, .. } => state
            .extra_watchers
            .read()
            .await
            .contains_key(&(chat_id, slot_id.clone())),
    }
}

/// Dispatches to the right `adopt_*` tail for a now-connected target.
async fn adopt_target(
    bot: &Bot,
    state: &AppState,
    chat_id: i64,
    target: &WatchTarget,
    client: JmapClient,
) -> bool {
    match target {
        WatchTarget::Primary => adopt_primary(bot, state, chat_id, client).await,
        WatchTarget::Shared { account_id, label } => {
            adopt_shared(
                bot,
                state,
                chat_id,
                account_id.clone(),
                label.clone(),
                client,
            )
            .await
        }
        WatchTarget::Extra { slot_id, label } => {
            adopt_extra(bot, state, chat_id, slot_id.clone(), label.clone(), client).await
        }
    }
}

/// The JMAP connect step each resume attempt goes through: the same
/// https-only, SSRF-guarded `jmap::connect`/`connect_shared` every live
/// flow (`/login`, `/partages`, `/comptes`) uses, dispatched by target
/// kind.
async fn connect_target(config: &Config, target: &ResumeTarget) -> anyhow::Result<JmapClient> {
    match &target.target {
        WatchTarget::Primary | WatchTarget::Extra { .. } => {
            jmap::connect(
                &target.server_url,
                &target.token,
                config.allow_private_jmap_hosts,
            )
            .await
        }
        WatchTarget::Shared { account_id, .. } => {
            jmap::connect_shared(
                &target.server_url,
                &target.token,
                config.allow_private_jmap_hosts,
                account_id,
            )
            .await
        }
    }
}

/// How a resume attempt turns credentials into a live `Client` — a trait
/// (rather than a direct call) so the retry loop can be exercised in
/// tests with canned failures and successes without standing up a real
/// TLS JMAP server. The production impl is just [`connect_target`]; same
/// reason jmap.rs's mock-server tests bypass that module's own wrapper.
trait ConnectJmap {
    async fn connect(
        &mut self,
        config: &Config,
        target: &ResumeTarget,
    ) -> anyhow::Result<JmapClient>;
}

struct RealConnect;

impl ConnectJmap for RealConnect {
    async fn connect(
        &mut self,
        config: &Config,
        target: &ResumeTarget,
    ) -> anyhow::Result<JmapClient> {
        connect_target(config, target).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use jmap_client::client::Credentials;
    use serde_json::json;
    use teloxide::adaptors::throttle::Limits;
    use teloxide::requests::RequesterExt;
    use teloxide::types::ChatId;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A well-formed-looking but fake token: teloxide defers authentication
    /// to request time, so merely building the bot performs no network I/O
    /// (none of these tests ever deliver a Telegram message).
    fn test_bot() -> Bot {
        teloxide::Bot::new("123456:TEST_TOKEN").throttle(Limits::default())
    }

    /// An `AppState` pair around a throwaway encrypted store seeded with
    /// `account` under chat 42.
    async fn state_with_account(account: Account) -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(Config {
            telegram_token: "123456:TEST_TOKEN".to_string(),
            authorized_chat_ids: HashSet::from([ChatId(42)]),
            data_dir: dir.path().to_path_buf(),
            // Loopback JMAP URLs are deliberate in these tests (unreachable
            // connect targets, wiremock); the SSRF guard would just get in
            // the way of what's being observed.
            allow_private_jmap_hosts: true,
            timezone: chrono_tz::Tz::UTC,
        });
        let store = Arc::new(crate::store::Store::open(dir.path()).unwrap());
        store.set(42, account).unwrap();
        (AppState::new(config, store), dir)
    }

    /// The persisted-account fixture: a primary mailbox, one shared
    /// account and one extra account, all pointing at a loopback port
    /// nothing listens on, so a real `jmap::connect` against it fails
    /// fast (connection refused) instead of timing out.
    fn unreachable_account() -> Account {
        Account {
            server_url: "https://127.0.0.1:9".to_string(),
            token: "tok".to_string(),
            email: "a@b.c".to_string(),
            last_state: Some("primary-state".to_string()),
            shared_accounts: HashMap::from([(
                "acc7".to_string(),
                crate::store::SharedAccount {
                    name: "shared@b.c".to_string(),
                    last_state: Some("shared-state".to_string()),
                },
            )]),
            extra_accounts: HashMap::from([(
                "slot1".to_string(),
                crate::store::ExtraAccount {
                    server_url: "https://127.0.0.1:9".to_string(),
                    token: "other-tok".to_string(),
                    email: "second@other.example".to_string(),
                    last_state: Some("extra-state".to_string()),
                },
            )]),
            muted: Vec::new(),
        }
    }

    /// Polls `pred` until it holds, with a generous real-time budget for
    /// slow CI runners.
    async fn eventually(budget: Duration, pred: impl Fn() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if pred() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A mock JMAP session endpoint, as jmap.rs's tests build one — enough
    /// for `Client::connect` to negotiate a session and hand back a Client
    /// whose watcher will then (harmlessly) fail its EventSource against
    /// the unmocked eventSourceUrl and keep retrying.
    async fn mock_jmap_client() -> JmapClient {
        let server = MockServer::start().await;
        let uri = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/jmap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(session_body(&uri)))
            .mount(&server)
            .await;
        JmapClient::new()
            .credentials(Credentials::bearer("test-token-123"))
            .connect(&uri)
            .await
            .expect("mock session negotiation should succeed")
    }
    fn session_body(mock_uri: &str) -> serde_json::Value {
        json!({
            "capabilities": {
                "urn:ietf:params:jmap:core": {
                    "maxSizeUpload": 50000000,
                    "maxConcurrentUpload": 4,
                    "maxSizeRequest": 10000000,
                    "maxConcurrentRequests": 4,
                    "maxCallsInRequest": 16,
                    "maxObjectsInGet": 500,
                    "maxObjectsInSet": 500,
                    "collationAlgorithms": []
                }
            },
            "accounts": {
                "acc1": {
                    "name": "user@example.org",
                    "isPersonal": true,
                    "isReadOnly": false,
                    "accountCapabilities": {}
                }
            },
            "primaryAccounts": { "urn:ietf:params:jmap:core": "acc1" },
            "username": "user@example.org",
            "apiUrl": format!("{mock_uri}/jmap/api"),
            "downloadUrl": format!("{mock_uri}/jmap/download/{{accountId}}/{{blobId}}/{{name}}"),
            "uploadUrl": format!("{mock_uri}/jmap/upload/{{accountId}}"),
            "eventSourceUrl": format!("{mock_uri}/jmap/eventsource"),
            "state": "session-state-1"
        })
    }

    #[test]
    fn persisted_targets_lists_primary_shared_and_extra_with_their_credentials() {
        let targets = persisted_targets(&unreachable_account());
        let [primary, shared, extra] = targets.as_slice() else {
            panic!("expected exactly 3 targets, got {targets:?}");
        };
        // Primary and shared ride the account's primary credentials;
        // the extra account carries its own (possibly different server).
        assert!(matches!(primary.target, WatchTarget::Primary));
        assert_eq!(primary.server_url, "https://127.0.0.1:9");
        assert_eq!(primary.token, "tok");
        match &shared.target {
            WatchTarget::Shared { account_id, label } => {
                assert_eq!(account_id, "acc7");
                assert_eq!(label, "shared@b.c");
            }
            other => panic!("expected a shared target, got {other:?}"),
        }
        assert_eq!(shared.server_url, primary.server_url);
        assert_eq!(shared.token, primary.token);
        match &extra.target {
            WatchTarget::Extra { slot_id, label } => {
                assert_eq!(slot_id, "slot1");
                assert_eq!(label, "second@other.example");
            }
            other => panic!("expected an extra target, got {other:?}"),
        }
        assert_eq!(extra.server_url, "https://127.0.0.1:9");
        assert_eq!(extra.token, "other-tok");
    }

    #[tokio::test]
    async fn boot_registration_marks_every_persisted_target_not_ready_before_any_connect() {
        // Regression test for the silent-green-boot bug: a dead JMAP server
        // at boot used to mean the account was never registered at all, so
        // /readyz answered 200 for a bot that was silently watching
        // nothing. Registration must happen before any connect attempt —
        // verified here with zero connects having run at all.
        let (state, _dir) = state_with_account(unreachable_account()).await;
        let targets = persisted_targets(&state.store.get(42).unwrap());
        register_persisted_targets(&state, 42, &targets);

        let snapshot = state.health.snapshot();
        assert_eq!(snapshot.len(), 3, "{snapshot:?}");
        let mut disconnected: Vec<String> = snapshot
            .iter()
            .filter(|(_, s)| !s.connected)
            .map(|(k, _)| k.clone())
            .collect();
        disconnected.sort();
        assert_eq!(
            disconnected,
            vec![
                "42/extra-slot1".to_string(),
                "42/primary".to_string(),
                "42/shared-acc7".to_string(),
            ],
            "readiness must name exactly the three registered targets"
        );
        // And through the readiness contract itself: not ready, naming
        // every persisted target.
        let Err(failing) = state.health.readiness() else {
            panic!("a registered-but-never-connected account must hold /readyz at 503");
        };
        let mut failing = failing;
        failing.sort();
        assert_eq!(failing, disconnected);
    }

    #[tokio::test]
    async fn a_failing_connect_keeps_the_target_registered_and_not_ready() {
        // The connect step here is the very `jmap::connect` boot resume
        // uses, against the account's real (unreachable) server URL — the
        // failure must leave the registration intact, not silently drop
        // the account back out of the registry.
        let (state, _dir) = state_with_account(unreachable_account()).await;
        let targets = persisted_targets(&state.store.get(42).unwrap());
        register_persisted_targets(&state, 42, &targets);

        let err = jmap::connect("https://127.0.0.1:9", "tok", true)
            .await
            .err()
            .expect("nothing listens on port 9, connect must fail");
        // Loopback is opted into via the test config's
        // allow_private_jmap_hosts, so the failure is the transport, not
        // the SSRF guard — matching a "server down" boot, which is what
        // the retry loop must survive.
        assert!(!err.to_string().contains("non publique"), "{err}");

        let snapshot = state.health.snapshot();
        assert_eq!(snapshot.len(), 3, "{snapshot:?}");
        assert!(snapshot.values().all(|s| !s.connected), "{snapshot:?}");
        assert!(state.health.readiness().is_err());
    }

    /// Canned connect step for driving the retry loop: fails a fixed
    /// number of times (or forever, with `usize::MAX`), counts every
    /// attempt, and on success connects to a real mock JMAP session so
    /// the adopted client is a genuine connected `Client`.
    struct FlakyConnect {
        failures_left: usize,
        attempts: Arc<AtomicUsize>,
    }

    impl ConnectJmap for FlakyConnect {
        async fn connect(
            &mut self,
            _config: &Config,
            _target: &ResumeTarget,
        ) -> anyhow::Result<JmapClient> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.failures_left > 0 {
                self.failures_left -= 1;
                return Err(anyhow::anyhow!("server still down (canned failure)"));
            }
            Ok(mock_jmap_client().await)
        }
    }

    #[tokio::test]
    async fn boot_resume_keeps_retrying_without_deregistering_while_the_server_is_down() {
        // Two attempts means at least one full failure → sleep → retry
        // cycle happened: the first failure must neither give up (the old
        // behavior — "it will need /login again") nor drop the health
        // entry (the silent-green-boot bug).
        let (state, _dir) = state_with_account(unreachable_account()).await;
        let targets = persisted_targets(&state.store.get(42).unwrap());
        register_persisted_targets(&state, 42, &targets);
        // Drive only the primary target to keep the assertion surface
        // small; the loop is shared by all three kinds.
        let primary = targets
            .into_iter()
            .find(|t| matches!(t.target, WatchTarget::Primary))
            .expect("primary target");

        let attempts = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(resume_target_with(
            test_bot(),
            state.clone(),
            42,
            primary,
            FlakyConnect {
                failures_left: usize::MAX,
                attempts: attempts.clone(),
            },
        ));

        // One retry costs 2s of real backoff (INITIAL_BACKOFF), so a 10s
        // budget is generous even on a loaded CI runner.
        assert!(
            eventually(Duration::from_secs(10), || attempts.load(Ordering::SeqCst)
                >= 2)
            .await,
            "resume loop must still be retrying after its first failure"
        );
        let snapshot = state.health.snapshot();
        assert!(snapshot.contains_key("42/primary"), "{snapshot:?}");
        assert!(!snapshot["42/primary"].connected, "{snapshot:?}");
        // No adoption happened — the client map stays empty while the
        // server is down.
        assert!(state.clients.read().await.is_empty());
        // And /readyz keeps naming every dead account (all three targets
        // of the account stayed registered; only the primary's loop is
        // being driven here).
        let Err(failing) = state.health.readiness() else {
            panic!("/readyz must keep naming the dead account");
        };
        let mut failing = failing;
        failing.sort();
        assert_eq!(
            failing,
            vec![
                "42/extra-slot1".to_string(),
                "42/primary".to_string(),
                "42/shared-acc7".to_string(),
            ]
        );
        task.abort();
    }

    #[tokio::test]
    async fn boot_resume_adopts_the_account_when_the_server_comes_back() {
        // One canned failure, then a working mock session: the loop must
        // ride out the failure on backoff and finally adopt — registering
        // the watcher, caching the client, and handing the health entry to
        // the watcher under the same key boot used.
        let (state, _dir) = state_with_account(unreachable_account()).await;
        let targets = persisted_targets(&state.store.get(42).unwrap());
        register_persisted_targets(&state, 42, &targets);
        let primary = targets
            .into_iter()
            .find(|t| matches!(t.target, WatchTarget::Primary))
            .expect("primary target");

        let attempts = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(resume_target_with(
            test_bot(),
            state.clone(),
            42,
            primary,
            FlakyConnect {
                failures_left: 1,
                attempts,
            },
        ));
        // One 2s backoff between the failure and the successful attempt.
        tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("resume task must finish after adopting")
            .expect("resume task must not panic");

        assert!(state.clients.read().await.contains_key(&42));
        assert!(state.watchers.read().await.contains_key(&42));
        let snapshot = state.health.snapshot();
        assert!(snapshot.contains_key("42/primary"), "{snapshot:?}");
        // The watcher owns the entry from here; connected reflects its own
        // EventSource, not the connect we just did.

        // And the full-chat teardown still cleans it all up — the
        // watchpoint this whole branch is about.
        state.forget(42).await;
        assert!(!state.health.snapshot().contains_key("42/primary"));
        assert!(state.clients.read().await.is_empty());
    }

    #[tokio::test]
    async fn boot_resume_gives_up_cleanly_when_the_account_is_removed_mid_retry() {
        // A /logout racing a boot-resume retry: the account vanishes from
        // the store while the connect keeps failing, and the retry task
        // must exit and drop its health entry rather than retry forever
        // over a removed account.
        let (state, _dir) = state_with_account(unreachable_account()).await;
        let targets = persisted_targets(&state.store.get(42).unwrap());
        register_persisted_targets(&state, 42, &targets);
        let primary = targets
            .into_iter()
            .find(|t| matches!(t.target, WatchTarget::Primary))
            .expect("primary target");

        let attempts = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(resume_target_with(
            test_bot(),
            state.clone(),
            42,
            primary,
            FlakyConnect {
                failures_left: usize::MAX,
                attempts: attempts.clone(),
            },
        ));
        assert!(
            eventually(Duration::from_secs(10), || attempts.load(Ordering::SeqCst)
                >= 1)
            .await,
            "at least one failed attempt must have run"
        );
        state.store.remove(42).unwrap();

        // The task exits on its next loop iteration (at most one backoff
        // interval away) and the entry goes with it.
        tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("resume task must exit once the account is removed")
            .expect("resume task must not panic");
        assert!(!state.health.snapshot().contains_key("42/primary"));
    }

    #[tokio::test]
    async fn adopt_is_idempotent_for_every_slot_kind() {
        // Two concurrent adoptions of the same slot must not spawn two
        // watchers — this is the guard that lets boot-resume retry
        // concurrently with /partages and /login without double-watching
        // (and double-notifying) an account.
        let (state, _dir) = state_with_account(unreachable_account()).await;
        let bot = test_bot();

        assert!(
            adopt_primary(&bot, &state, 42, mock_jmap_client().await).await,
            "first adoption must succeed"
        );
        assert!(
            !adopt_primary(&bot, &state, 42, mock_jmap_client().await).await,
            "second adoption of the same chat must be refused"
        );
        assert!(
            adopt_shared(
                &bot,
                &state,
                42,
                "acc7".to_string(),
                "shared@b.c".to_string(),
                mock_jmap_client().await,
            )
            .await,
            "first shared adoption must succeed"
        );
        assert!(
            !adopt_shared(
                &bot,
                &state,
                42,
                "acc7".to_string(),
                "a different label".to_string(),
                mock_jmap_client().await,
            )
            .await,
            "second adoption of the same shared account must be refused"
        );
        assert!(
            adopt_extra(
                &bot,
                &state,
                42,
                "slot1".to_string(),
                "second@other.example".to_string(),
                mock_jmap_client().await,
            )
            .await,
            "first extra adoption must succeed"
        );
        assert!(
            !adopt_extra(
                &bot,
                &state,
                42,
                "slot1".to_string(),
                "another label".to_string(),
                mock_jmap_client().await,
            )
            .await,
            "second adoption of the same extra slot must be refused"
        );

        // Exactly one handle and one client per slot, and one health entry
        // per target, keyed the way the watcher itself keys them.
        assert_eq!(state.watchers.read().await.len(), 1);
        assert_eq!(state.shared_watchers.read().await.len(), 1);
        assert_eq!(state.extra_watchers.read().await.len(), 1);
        assert_eq!(state.clients.read().await.len(), 1);
        assert_eq!(state.shared_clients.read().await.len(), 1);
        assert_eq!(state.extra_clients.read().await.len(), 1);
        let snapshot = state.health.snapshot();
        let mut keys: Vec<_> = snapshot.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "42/extra-slot1".to_string(),
                "42/primary".to_string(),
                "42/shared-acc7".to_string(),
            ],
            "{snapshot:?}"
        );
    }
}
