//! Test-only support: an in-memory `ControlPlane` mock + relay-state fixtures,
//! so the relay's routing/KEX/presence logic is tested without Docker.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kallip_agora_common::bytes::Ed25519PublicKey;
use kallip_agora_common::control_plane::{
    ControlPlane, ControlPlaneError, TagmaProfile, UserIdentity, VerifiedSession,
};
use kallip_agora_common::ids::{TagmaId, UserId};
use kallip_agora_common::principal::Principal;

use crate::state::{ConversationsState, SharedConvState};

/// An in-memory `ControlPlane`. No Docker: tagmas, tokens, sessions, and the
/// replay high-water-mark all live in `Mutex<HashMap>`s the test seeds directly.
pub struct MockControlPlane {
    tagmas: Mutex<HashMap<TagmaId, MockTagma>>,
    /// bearer token -> tagma it authenticates as.
    tokens: Mutex<HashMap<String, TagmaId>>,
    /// session cookie value -> user.
    sessions: Mutex<HashMap<String, UserId>>,
    replay_ts: Mutex<HashMap<TagmaId, i64>>,
    /// Known user accounts: user id -> display identity + disabled flag. The
    /// `tagma_profiles` resolver derives each tagma's `owner_disabled` from its
    /// owner's entry here, matching prod (the disabled bit lives on the user).
    users: Mutex<HashMap<UserId, MockUser>>,
}

struct MockTagma {
    owner: UserId,
    pinned_key: Option<Ed25519PublicKey>,
    enrolled: bool,
    revoked: bool,
    label: Option<String>,
    owner_username: String,
    owner_display_name: Option<String>,
}

struct MockUser {
    username: String,
    display_name: Option<String>,
    disabled: bool,
}

impl MockControlPlane {
    pub fn new() -> Self {
        Self {
            tagmas: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            replay_ts: Mutex::new(HashMap::new()),
            users: Mutex::new(HashMap::new()),
        }
    }

    /// Seed an enrolled tagma owned by `owner` with the given pinned key, and a
    /// bearer `token` that authenticates as it. Auto-seeds the owner user row if
    /// absent so the mock mirrors prod's FK-guaranteed owner (a missing owner is
    /// never silently permissive -- `owner_disabled` always reflects a real row).
    pub fn enroll_tagma(
        &self,
        tagma: &TagmaId,
        owner: UserId,
        pinned_key: Ed25519PublicKey,
        token: &str,
    ) {
        let owner_username = owner.as_ref().to_string();
        let mut users = self.users.lock().unwrap();
        users.entry(owner.clone()).or_insert(MockUser {
            username: owner_username.clone(),
            display_name: None,
            disabled: false,
        });
        drop(users);
        self.tagmas.lock().unwrap().insert(
            tagma.clone(),
            MockTagma {
                owner_username,
                owner,
                pinned_key: Some(pinned_key),
                enrolled: true,
                revoked: false,
                label: None,
                owner_display_name: None,
            },
        );
        self.tokens
            .lock()
            .unwrap()
            .insert(token.to_string(), tagma.clone());
    }

    /// Override a seeded tagma's pinned key (`None` = pending / not yet
    /// enrolled-a-key), so a test can drive the tunnel / rooms-send gate's
    /// no-pinned-key branch.
    pub fn set_pinned_key(&self, tagma: &TagmaId, key: Option<Ed25519PublicKey>) {
        let mut tagmas = self.tagmas.lock().unwrap();
        let t = tagmas
            .get_mut(tagma)
            .expect("set_pinned_key: tagma must be enrolled first");
        t.pinned_key = key;
    }

    /// Override a seeded tagma's owner-set label, so a roster test can assert the
    /// resolved display name.
    pub fn set_tagma_label(&self, tagma: &TagmaId, label: Option<String>) {
        let mut tagmas = self.tagmas.lock().unwrap();
        let t = tagmas
            .get_mut(tagma)
            .expect("set_tagma_label: tagma must be enrolled first");
        t.label = label;
    }

    /// Seed a known user account with the mock's default identity: username =
    /// the user id string, no display name, not disabled. Used by the
    /// room-management handler tests.
    pub fn seed_user(&self, user: UserId) {
        let id_str = user.as_ref().to_string();
        self.users.lock().unwrap().insert(
            user,
            MockUser {
                username: id_str,
                display_name: None,
                disabled: false,
            },
        );
    }

    /// Seed a known user account with an explicit display identity (for the
    /// roster `user_identities` resolve, which needs a real username + display
    /// name). Used by the roster-display test.
    #[allow(dead_code)]
    pub fn seed_user_with(&self, user: UserId, username: &str, display_name: Option<&str>) {
        self.users.lock().unwrap().insert(
            user,
            MockUser {
                username: username.to_string(),
                display_name: display_name.map(str::to_string),
                disabled: false,
            },
        );
    }

    /// Mark a seeded user account disabled (mirrors `users.disabled_at`), so the
    /// invite gate (`!disabled`) and the tagma `owner_disabled` fact reflect it.
    /// Panics if the user was not seeded -- a test typo should fail fast, not
    /// pass vacuously.
    pub fn disable_user(&self, user: &UserId) {
        let mut users = self.users.lock().unwrap();
        let u = users
            .get_mut(user)
            .expect("disable_user: user must be seeded first");
        u.disabled = true;
    }

    /// Mark a seeded tagma revoked (mirrors `tagmata.revoked_at`). Panics if the
    /// tagma was not enrolled -- fail fast on a test typo.
    pub fn revoke_tagma(&self, tagma: &TagmaId) {
        let mut tagmas = self.tagmas.lock().unwrap();
        let t = tagmas
            .get_mut(tagma)
            .expect("revoke_tagma: tagma must be enrolled first");
        t.revoked = true;
    }
}

#[async_trait::async_trait]
impl ControlPlane for MockControlPlane {
    async fn verify_session(
        &self,
        cookie_value: &str,
    ) -> Result<Option<VerifiedSession>, ControlPlaneError> {
        // The mock stores only the user id; synthesize a display from it
        // (test-only — the real impl resolves username/display_name from the
        // users row alongside the auth check).
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .get(cookie_value)
            .cloned()
            .map(|user_id| VerifiedSession {
                username: user_id.to_string(),
                display_name: None,
                user_id,
            }))
    }

    async fn verify_bearer(&self, token: &str) -> Result<Option<Principal>, ControlPlaneError> {
        let Some(tagma) = self.tokens.lock().unwrap().get(token).cloned() else {
            return Ok(None);
        };
        let tagmas = self.tagmas.lock().unwrap();
        let Some(t) = tagmas.get(&tagma) else {
            return Ok(None);
        };
        if t.revoked {
            return Ok(None);
        }
        Ok(Some(Principal::Tagma(tagma)))
    }

    async fn tagma_profiles(
        &self,
        tagma_ids: &[TagmaId],
    ) -> Result<Vec<TagmaProfile>, ControlPlaneError> {
        // UNFILTERED, matching prod: return every existing input tagma with its
        // raw facts. `owner_disabled` is derived from the owner's seeded user
        // entry (default false when the owner is unseeded).
        let tagmas = self.tagmas.lock().unwrap();
        let users = self.users.lock().unwrap();
        let mut out = Vec::new();
        for id in tagma_ids {
            let Some(t) = tagmas.get(id) else {
                continue;
            };
            let owner_disabled = users.get(&t.owner).is_some_and(|u| u.disabled);
            out.push(TagmaProfile {
                tagma_id: id.clone(),
                pinned_public_key: t.pinned_key.clone(),
                owner_user_id: t.owner.clone(),
                label: t.label.clone(),
                owner_username: t.owner_username.clone(),
                owner_display_name: t.owner_display_name.clone(),
                enrolled: t.enrolled,
                revoked: t.revoked,
                owner_disabled,
            });
        }
        Ok(out)
    }

    async fn user_identities(
        &self,
        user_ids: &[UserId],
    ) -> Result<Vec<UserIdentity>, ControlPlaneError> {
        // UNFILTERED: return every existing input user with its raw `disabled`
        // flag.
        let users = self.users.lock().unwrap();
        let mut out = Vec::new();
        for id in user_ids {
            let Some(u) = users.get(id) else {
                continue;
            };
            out.push(UserIdentity {
                user_id: id.clone(),
                username: u.username.clone(),
                display_name: u.display_name.clone(),
                disabled: u.disabled,
            });
        }
        Ok(out)
    }

    async fn user_identity_by_username(
        &self,
        username: &str,
    ) -> Result<Option<UserIdentity>, ControlPlaneError> {
        // Linear scan by stored username. The fake stores usernames verbatim
        // (seeded by the test), so a direct -- case-sensitive -- match mirrors
        // "this handle resolves to this user" without reimplementing the
        // registry's normalizer. No seeded match -> None (the invite gate's
        // 404).
        let users = self.users.lock().unwrap();
        Ok(users
            .iter()
            .find(|(_, u)| u.username == username)
            .map(|(id, u)| UserIdentity {
                user_id: id.clone(),
                username: u.username.clone(),
                display_name: u.display_name.clone(),
                disabled: u.disabled,
            }))
    }

    async fn bump_tunnel_proof_ts(
        &self,
        tagma_id: &TagmaId,
        ts: i64,
    ) -> Result<bool, ControlPlaneError> {
        let mut replay = self.replay_ts.lock().unwrap();
        let fresh = replay.get(tagma_id).copied().is_none_or(|prev| prev < ts);
        if fresh {
            replay.insert(tagma_id.clone(), ts);
        }
        Ok(fresh)
    }
}

/// Build a `SharedConvState` wired to a fresh mock registry. The mock is
/// returned so the test can seed tagmas/tokens.
pub fn make_state(
    proof_skew_secs: i64,
    key_exchange_timeout: std::time::Duration,
) -> (SharedConvState, Arc<MockControlPlane>) {
    let control = Arc::new(MockControlPlane::new());
    let state: SharedConvState = Arc::new(ConversationsState {
        control: control.clone(),
        registry: std::sync::RwLock::new(crate::state::Registry::new()),
        pending_key_exchange: std::sync::Mutex::new(HashMap::new()),
        proof_skew_secs,
        key_exchange_timeout,
        db: None,
        agent_profiles: crate::state::AgentProfileCache::default(),
    });
    (state, control)
}

/// Insert a presence entry directly (bypassing the tunnel handler's proof
/// machinery) and return the per-connection identity token + the tunnel's
/// inbound broadcast sender. Mirrors what a live tagma tunnel establishes.
pub fn seed_presence(
    state: &SharedConvState,
    tagma: &TagmaId,
    owner: UserId,
) -> (
    tokio::sync::broadcast::Sender<kallip_lesche_common::tunnel::TunnelInbound>,
    Arc<()>,
) {
    let (tx, _rx) =
        tokio::sync::broadcast::channel::<kallip_lesche_common::tunnel::TunnelInbound>(128);
    let id = Arc::new(());
    let mut reg = state.registry.write().unwrap();
    reg.register_presence(tagma, owner, tx.clone(), id.clone());
    (tx, id)
}

/// The shared test Postgres: one reusable container per machine, named
/// `kallipai-testcontainers-pg-16alpine`, so every test binary in the workspace reuses
/// the same server instead of racing `start()` calls against RootlessKit's
/// host-port window. Bump the tag => pick a new name (or `docker rm -f` the
/// old one) or the reused container keeps the stale version.
static SHARED_PG_PORT: tokio::sync::OnceCell<u16> = tokio::sync::OnceCell::const_new();

/// Monotonic counter for unique per-test database names.
static DB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn shared_pg_port() -> &'static u16 {
    SHARED_PG_PORT
        .get_or_init(|| async {
            use testcontainers_modules::testcontainers::ImageExt;
            use testcontainers_modules::testcontainers::ReuseDirective;
            use testcontainers_modules::testcontainers::runners::AsyncRunner;
            // Twin of kallip-agora's test_helpers::shared_pg_port (same
            // request: name + tag + credentials) so both binaries reuse
            // ONE container; keep the two copies in sync.
            let make_request = || {
                testcontainers_modules::postgres::Postgres::default()
                    .with_db_name("postgres")
                    .with_user("postgres")
                    .with_password("postgres")
                    .with_tag("16-alpine")
                    .with_container_name("kallipai-testcontainers-pg-16alpine")
                    .with_reuse(ReuseDirective::Always)
            };
            // Seatbelt for the rare cold start (the container is missing and
            // an ephemeral outbound source port snatches RootlessKit's picked
            // port). Steady state reuses the running container and never
            // reaches this loop's error path. The retry matches on the
            // error's display string -- fragile, but test-only and the
            // cheapest available signal for this docker error chain.
            for attempt in 0..4 {
                match make_request().start().await {
                    Ok(container) => {
                        let port = container.get_host_port_ipv4(5432).await.expect("host port");
                        sweep_dead_test_dbs(port).await;
                        // Keep the container alive for the whole test process.
                        std::mem::forget(container);
                        return port;
                    }
                    Err(e) if attempt < 3 && e.to_string().contains("address already in use") => {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                    Err(e) => panic!("start postgres: {e}"),
                }
            }
            unreachable!("retry loop always returns or panics")
        })
        .await
}

/// Best-effort cleanup, once per process, of test databases left dead by
/// earlier runs (the reusable container never drops them). Sweeps BOTH
/// crates' prefixes: the shared container's hygiene must not depend on
/// which crate is running. Twin of kallip-agora's
/// test_helpers::sweep_dead_test_dbs.
async fn sweep_dead_test_dbs(port: u16) {
    use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    // Hygiene, not a correctness path: on any failure just leave the dead
    // databases to accumulate, exactly as before this sweep existed.
    let root = match Database::connect(&url).await {
        Ok(root) => root,
        Err(e) => {
            eprintln!("dead-db sweep: connect failed: {e}");
            return;
        }
    };
    // Zero-connection criterion. Known boundary, unreachable under cargo
    // test's serialized binaries: a concurrently cold-started sibling's
    // just-created, not-yet-connected database could be swept -- same
    // class as the container name-conflict note. This process's own
    // databases appear only after the sweep and hold connections once
    // live, so only dead databases ever match.
    // DROP DATABASE cannot run inside a transaction block, so a DO-block
    // sweep silently drops nothing; select candidates first, then issue
    // one autocommit DROP per database, each failure swallowed.
    let candidates = match root
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT datname FROM pg_database d \
             WHERE d.datname ~ '^(lesche|agora)_test_' \
             AND NOT EXISTS \
             (SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname)"
                .to_owned(),
        ))
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("dead-db sweep: list failed: {e}");
            return;
        }
    };
    for name in candidates
        .iter()
        .filter_map(|row| row.try_get::<String>("", "datname").ok())
    {
        let quoted = format!("\"{}\"", name.replace('"', "\"\""));
        let drop = format!("DROP DATABASE IF EXISTS {quoted}");
        if let Err(e) = root
            .execute(Statement::from_string(DatabaseBackend::Postgres, drop))
            .await
        {
            eprintln!("dead-db sweep: drop {name} failed: {e}");
        }
    }
}

/// Provision a fresh, isolated database on the shared Postgres and run the
/// lesche migrations. Parallel-safe: each call carves out a database named
/// after the process and a per-process counter, and defensively drops a
/// stale same-named database first (pid reuse). Dead databases disappear
/// with the container (`docker rm -f kallipai-testcontainers-pg-16alpine`); no other
/// cleanup exists by design.
pub(crate) async fn provision_test_db() -> crate::db::Db {
    use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
    let port = *shared_pg_port().await;
    let n = DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let db_name = format!("lesche_test_{}_{n}", std::process::id());
    let root_url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let root = Database::connect(&root_url)
        .await
        .expect("connect to postgres maintenance db");
    root.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("DROP DATABASE IF EXISTS \"{db_name}\""),
    ))
    .await
    .expect("drop stale test database");
    root.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("CREATE DATABASE \"{db_name}\""),
    ))
    .await
    .expect("create test database");
    drop(root);
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/{db_name}");
    crate::db::connect_and_migrate(&url)
        .await
        .expect("connect + migrate")
}
