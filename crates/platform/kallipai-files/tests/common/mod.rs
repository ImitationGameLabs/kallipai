//! Shared fixtures for the HTTP integration tests: a per-process Postgres
//! (mirroring the unit-test harness, separate container) plus an in-memory
//! `ControlPlane` mock and a router builder. Needs Docker at test time.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kallipai_archeion_common::control_plane::{
    ControlPlane, ControlPlaneError, EnrollmentLookup, TagmaProfile, UserIdentity, VerifiedSession,
};
use kallipai_archeion_common::ids::{TagmaId, UserId};
use kallipai_archeion_common::principal::Principal;
use kallipai_files::LocalBackend;
use kallipai_files::gc::GcConfig;
use kallipai_files::metadata::Db;
use kallipai_files::migration::Migrator;
use kallipai_files::state::{AppState, FilesConfig};
use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use sea_orm_migration::MigratorTrait;
use tempfile::TempDir;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ImageExt, ReuseDirective};
use tokio::sync::OnceCell;

static SHARED_PG_PORT: OnceCell<u16> = OnceCell::const_new();
static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

const CONTAINER_NAME: &str = "kallipai-testcontainers-pg-files-api";
const DB_PREFIX: &str = "files_api_test_";

fn retryable_start_error(e: &str) -> bool {
    e.contains("already in use")
}

/// A test database is owned by the process whose pid is encoded in its
/// name (`{DB_PREFIX}{pid}_{n}`). The zero-connection SQL prefilter
/// cannot tell "dead" from "just created, not yet connected", so a
/// candidate is dropped only when its owner process is gone
/// (`/proc/{pid}`). Unparseable names are never dropped.
fn owner_dead(db_name: &str) -> bool {
    let Some(pid) = db_name
        .strip_prefix(DB_PREFIX)
        .and_then(|rest| rest.split_once('_'))
        .and_then(|(pid, _)| pid.parse::<u32>().ok())
    else {
        return false;
    };
    !std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Best-effort cleanup, once per process, of test databases left dead by
/// earlier runs: the reusable container never drops them, so dead
/// databases accumulate across runs without this. Same hygiene contract
/// as the unit-test harness's sweep. DROP DATABASE cannot run inside a
/// transaction block, so candidates are selected first, then one
/// autocommit DROP per database, each failure swallowed.
async fn sweep_dead_test_dbs(port: u16) {
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let root = match Database::connect(&url).await {
        Ok(root) => root,
        Err(e) => {
            eprintln!("dead-db sweep: connect failed: {e}");
            return;
        }
    };
    let candidates = match root
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT datname FROM pg_database d \
                 WHERE d.datname ~ '^{DB_PREFIX}' \
                 AND NOT EXISTS \
                 (SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname)"
            ),
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
        if !owner_dead(&name) {
            continue;
        }
        let quoted = format!("\"{name}\"");
        let drop = format!("DROP DATABASE IF EXISTS {quoted}");
        if let Err(e) = root
            .execute(Statement::from_string(DatabaseBackend::Postgres, drop))
            .await
        {
            eprintln!("dead-db sweep: drop {name} failed: {e}");
        }
    }
}

async fn shared_pg_port() -> u16 {
    *SHARED_PG_PORT
        .get_or_init(|| async {
            let make_request = || {
                Postgres::default()
                    .with_db_name("postgres")
                    .with_user("postgres")
                    .with_password("postgres")
                    .with_tag("16-alpine")
                    .with_container_name(CONTAINER_NAME)
                    .with_reuse(ReuseDirective::Always)
            };
            for attempt in 0..4 {
                match make_request().start().await {
                    Ok(container) => {
                        let port = container.get_host_port_ipv4(5432).await.expect("host port");
                        sweep_dead_test_dbs(port).await;
                        std::mem::forget(container);
                        return port;
                    }
                    Err(e) if attempt < 3 && retryable_start_error(&e.to_string()) => {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                    Err(e) => panic!("start postgres: {e}"),
                }
            }
            unreachable!("retry loop always returns or panics")
        })
        .await
}

/// One fresh, empty database per call; returns its URL. The caller owns
/// migration (the binary's `run` path does `connect_and_migrate` itself).
pub async fn test_db_url() -> String {
    let port = shared_pg_port().await;
    let n = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let db_name = format!("{DB_PREFIX}{}_{n}", std::process::id());
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let root = Database::connect(&url).await.expect("connect root");
    root.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("CREATE DATABASE {db_name}"),
    ))
    .await
    .expect("create test database");
    format!("postgres://postgres:postgres@127.0.0.1:{port}/{db_name}")
}

/// One fresh migrated database per call.
pub async fn test_db() -> Db {
    let db_url = test_db_url().await;
    let db = Database::connect(&db_url).await.expect("connect test db");
    Migrator::up(&db, None).await.expect("migrate");
    db
}
/// In-memory registry mock: users, sessions, tagma tokens, and one
/// enrollment-set entry per space member (each member's lookup resolves to
/// the same space), mirroring the registry's per-tagma read shape.
#[derive(Default)]
pub struct MockControlPlaneInner {
    pub sessions: HashMap<String, UserId>,
    pub tokens: HashMap<String, TagmaId>,
    pub admin_token: Option<String>,
    pub users: BTreeSet<String>,
    /// TagmaId -> (owning user, full enrolled set of that space).
    pub enrollments: HashMap<String, (String, BTreeSet<String>)>,
}

#[derive(Clone, Default)]
pub struct MockControlPlane(pub Arc<Mutex<MockControlPlaneInner>>);

impl MockControlPlane {
    /// Register a user and mint a session cookie for it.
    pub fn seed_session(&self, user: &UserId) -> String {
        let cookie = format!("sess-{}", uuid::Uuid::new_v4());
        let mut inner = self.0.lock().unwrap();
        inner.sessions.insert(cookie.clone(), user.clone());
        inner.users.insert(user.to_string());
        cookie
    }
    pub fn seed_admin(&self) -> String {
        let token = format!("sk-admin-{}", uuid::Uuid::new_v4());
        self.0.lock().unwrap().admin_token = Some(token.clone());
        token
    }
    pub fn revoke_session(&self, cookie: &str) {
        self.0.lock().unwrap().sessions.remove(cookie);
    }

    /// Enroll one tagma into a space: mints its token and folds it into
    /// every member's enrollment set.
    pub fn seed_tagma(&self, space_user: &UserId, members: &mut Vec<TagmaId>) -> (TagmaId, String) {
        let mut inner = self.0.lock().unwrap();
        let tagma_id = TagmaId::from(uuid::Uuid::new_v4().to_string());
        let token = format!("sk-tagma-{}", uuid::Uuid::new_v4());
        inner.tokens.insert(token.clone(), tagma_id.clone());
        members.push(tagma_id.clone());
        for member in members.iter() {
            inner.enrollments.insert(
                member.to_string(),
                (
                    space_user.to_string(),
                    members.iter().map(|m| m.to_string()).collect(),
                ),
            );
        }
        (tagma_id, token)
    }
}

#[async_trait::async_trait]
impl ControlPlane for MockControlPlane {
    async fn verify_session(
        &self,
        cookie_value: &str,
    ) -> Result<Option<VerifiedSession>, ControlPlaneError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .sessions
            .get(cookie_value)
            .map(|user_id| VerifiedSession {
                user_id: user_id.clone(),
                username: format!("user-{user_id}"),
                display_name: None,
                local_admin: false,
            }))
    }

    async fn verify_bearer(&self, token: &str) -> Result<Option<Principal>, ControlPlaneError> {
        let inner = self.0.lock().unwrap();
        if let Some(admin) = &inner.admin_token
            && token == admin
        {
            return Ok(Some(Principal::Admin));
        }
        Ok(inner.tokens.get(token).map(|t| Principal::Tagma(t.clone())))
    }

    async fn tagma_profiles(
        &self,
        _tagma_ids: &[TagmaId],
    ) -> Result<Vec<TagmaProfile>, ControlPlaneError> {
        Ok(Vec::new())
    }

    async fn user_identities(
        &self,
        user_ids: &[UserId],
    ) -> Result<Vec<UserIdentity>, ControlPlaneError> {
        let inner = self.0.lock().unwrap();
        Ok(user_ids
            .iter()
            .filter(|id| inner.users.contains(id.as_ref()))
            .map(|id| UserIdentity {
                user_id: id.clone(),
                username: format!("user-{id}"),
                display_name: None,
                disabled: false,
            })
            .collect())
    }

    async fn user_identity_by_username(
        &self,
        _username: &str,
    ) -> Result<Option<UserIdentity>, ControlPlaneError> {
        Ok(None)
    }

    async fn enrollment_lookup(
        &self,
        tagma_id: &TagmaId,
    ) -> Result<Option<EnrollmentLookup>, ControlPlaneError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .enrollments
            .get(tagma_id.as_ref())
            .map(|(user, members)| EnrollmentLookup {
                user_id: UserId::from(user.clone()),
                enrolled_tagmas: members.iter().map(|m| TagmaId::from(m.clone())).collect(),
            }))
    }

    async fn bump_tunnel_proof_ts(
        &self,
        _tagma_id: &TagmaId,
        _ts: i64,
    ) -> Result<bool, ControlPlaneError> {
        Ok(false)
    }
}

/// One test world: migrated DB, temp blob root, mock registry, router.
pub struct TestWorld {
    pub router: axum::Router,
    pub mock: MockControlPlane,
    pub user1: UserId,
    pub user1_cookie: String,
    pub user2: UserId,
    pub user2_cookie: String,
    pub t1: TagmaId,
    pub t1_token: String,
    pub t2: TagmaId,
    pub t2_token: String,
    pub t3: TagmaId,
    /// Read by list_tests (the cross-space pin); unused in api_tests.
    #[allow(dead_code)]
    pub t3_token: String,
    pub admin_token: String,
    /// Keeps the blob root alive for the whole test.
    pub blob_dir: TempDir,
}

impl TestWorld {
    pub async fn new() -> Self {
        let blob_dir = TempDir::new().expect("blob dir");
        let blob_root = blob_dir.path().to_path_buf();
        Self::with_parts(
            LocalBackend::arc(&blob_root),
            blob_root,
            1024 * 1024,
            blob_dir,
            None,
        )
        .await
    }

    /// Same world with a custom body cap (the underflow-window pin).
    pub async fn with_cap(max_body_bytes: u64) -> Self {
        let blob_dir = TempDir::new().expect("blob dir");
        let blob_root = blob_dir.path().to_path_buf();
        Self::with_parts(
            LocalBackend::arc(&blob_root),
            blob_root,
            max_body_bytes,
            blob_dir,
            None,
        )
        .await
    }

    /// Full control (the windowed-read evidence test swaps in a
    /// recording store): caller supplies the store, its root, the cap,
    /// and keeps the root's TempDir alive.
    pub async fn with_parts(
        blob: std::sync::Arc<dyn kallipai_files::BlobStore>,
        blob_root: PathBuf,
        max_body_bytes: u64,
        blob_dir: TempDir,
        notify: Option<Arc<dyn kallipai_files::notify::NotifyPusher>>,
    ) -> Self {
        let db = test_db().await;
        let mock = MockControlPlane::default();
        let user1 = UserId::from(uuid::Uuid::new_v4().to_string());
        let user2 = UserId::from(uuid::Uuid::new_v4().to_string());
        let user1_cookie = mock.seed_session(&user1);
        let user2_cookie = mock.seed_session(&user2);
        // t1, t2 enrolled in user1's space; t3 enrolled in user2's space.
        let mut members1 = Vec::new();
        let (t1, t1_token) = mock.seed_tagma(&user1, &mut members1);
        let (t2, t2_token) = mock.seed_tagma(&user1, &mut members1);
        let mut members2 = Vec::new();
        let (t3, t3_token) = mock.seed_tagma(&user2, &mut members2);
        let admin_token = mock.seed_admin();
        let state = AppState {
            db,
            blob,
            blob_root,
            control: Arc::new(mock.clone()),
            config: Arc::new(FilesConfig {
                max_body_bytes,
                cors_origins: String::new(),
                degrade_fail_soft: false,
                gc: GcConfig {
                    batch: 128,
                    interval: std::time::Duration::from_secs(3600),
                    grace: std::time::Duration::from_secs(3600),
                },
            }),
            notify,
        };
        let router = kallipai_files::state::router(state);
        Self {
            router,
            mock,
            user1,
            user1_cookie,
            user2,
            user2_cookie,
            t1,
            t1_token,
            t2,
            t2_token,
            t3,
            t3_token,
            admin_token,
            blob_dir,
        }
    }
}
