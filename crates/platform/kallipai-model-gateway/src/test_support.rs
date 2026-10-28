//! Test-only fixtures: an ephemeral per-process Postgres (testcontainers)
//! with one isolated database per test. Ported from the files harness; this
//! crate talks to its own container so gateway tests never contend with the
//! other services' containers. Needs Docker at test time.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ImageExt, ReuseDirective};
use tokio::sync::OnceCell;
use tracing_subscriber::prelude::*;

pub(crate) use kallipai_common::telemetry::test_observability::{ObservedSpan, Recorder};

use crate::db::Db;

/// The canonical test tagma bearer: the mock verifier admits it as the
/// tagma `tagma-under-test` (enrolled to `user-plain` by the fixture
/// default).
pub(crate) const TEST_TAGMA_BEARER: &str = "test-tagma-bearer-0123456789abcd";
/// The visibility fixture's `tagma-bound` bearer (the bound account's
/// tagma; the enrollment map binds it per-test).
pub(crate) const TEST_TAGMA_BEARER_BOUND: &str = "test-tagma-bearer-bound-012345678";
/// The visibility fixture's `tagma-other` bearer.
pub(crate) const TEST_TAGMA_BEARER_OTHER: &str = "test-tagma-bearer-other-01234567";

/// The upstream provider key seeded behind profile p1/p2/p3 credentials.
pub(crate) const UPSTREAM_KEY: &str = "sk-upstream-secret";
/// The admin bearer credential the mock verifier admits (Principal::Admin;
/// the machine channel's canonical test credential).
pub(crate) const TEST_ADMIN_BEARER: &str = "test-admin-bearer-token";

/// The session cookie the mock verifier admits (a local-admin session;
/// the browser channel's canonical test credential).
pub(crate) const TEST_ADMIN_COOKIE: &str = "test-admin-session-cookie";

/// A valid NON-admin bearer (Principal::User): exercises the 403
/// authenticated-but-unauthorized shape on the bearer channel.
pub(crate) const TEST_USER_BEARER: &str = "test-user-bearer-token";

/// A valid NON-admin session cookie (local_admin: false): exercises the
/// 403 shape on the cookie channel.
pub(crate) const TEST_USER_COOKIE: &str = "test-user-session-cookie";

/// Scripted [`crate::management::AuthVerifier`]: admits exactly
/// [`TEST_ADMIN_BEARER`] as the admin principal and [`TEST_ADMIN_COOKIE`]
/// as a local-admin session; every other credential is invalid.
/// `unreachable()` answers backend errors so the 503 fail-closed path is
/// exercisable.
fn default_tagma_tokens() -> std::collections::HashMap<String, String> {
    [(TEST_TAGMA_BEARER.to_owned(), "tagma-under-test".to_owned())]
        .into_iter()
        .collect()
}
pub(crate) struct MockVerifier {
    unreachable: bool,
    /// The tagma ids whose tokens the mock refuses (the archeion
    /// folds a disabled owner into a bare refusal); every other id
    /// answers not-disabled.
    disabled: std::sync::Mutex<Vec<String>>,
    /// When true, any tagma answers enrolled to `user-plain` -- the
    /// error-path verifiers keep resolution working without a map. The
    /// fixture default and the refusal-shape tests read the explicit
    /// `enrollment` map instead (wildcard off).
    enrollment_wildcard: bool,
    /// The explicit enrollment answers: tagma id to owning user id,
    /// served when the wildcard is off.
    enrollment: std::collections::HashMap<String, String>,
    /// The admitted tagma bearers: token to tagma id. The fixture
    /// default admits [`TEST_TAGMA_BEARER`] as `tagma-under-test`;
    /// tests with more tagmas extend it with [`with_tagmas`].
    tagma_tokens: std::collections::HashMap<String, String>,
    /// Every verify-bearer call, counted: the TTL-cache tests assert the
    /// second request rides the cache instead of reprobing here.
    verify_calls: std::sync::atomic::AtomicUsize,
}

impl MockVerifier {
    pub(crate) fn unreachable() -> Self {
        Self {
            unreachable: true,
            disabled: std::sync::Mutex::new(Vec::new()),
            enrollment_wildcard: true,
            enrollment: std::collections::HashMap::new(),
            tagma_tokens: default_tagma_tokens(),
            verify_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// A verifier answering owner-disabled for exactly `ids`.
    pub(crate) fn disabling(ids: &[&str]) -> Self {
        Self {
            unreachable: false,
            disabled: std::sync::Mutex::new(ids.iter().map(|s| (*s).to_owned()).collect()),
            enrollment_wildcard: true,
            enrollment: std::collections::HashMap::new(),
            tagma_tokens: default_tagma_tokens(),
            verify_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// A verifier answering enrollment from exactly `pairs`: each
    /// listed (tagma, owner) addresses, every other id refuses.
    pub(crate) fn enrolled(pairs: &[(&str, &str)]) -> Self {
        Self {
            unreachable: false,
            disabled: std::sync::Mutex::new(Vec::new()),
            enrollment_wildcard: false,
            enrollment: pairs
                .iter()
                .map(|(t, u)| ((*t).to_owned(), (*u).to_owned()))
                .collect(),
            tagma_tokens: default_tagma_tokens(),
            verify_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The verify-bearer call count so far (the cache tests read it).
    pub(crate) fn verify_bearer_calls(&self) -> usize {
        self.verify_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Extend the admitted tagma bearers (token to tagma id); the
    /// fixture default stays admitted.
    pub(crate) fn with_tagmas(mut self, bearers: &[(&str, &str)]) -> Self {
        self.tagma_tokens.extend(
            bearers
                .iter()
                .map(|(t, id)| ((*t).to_owned(), (*id).to_owned())),
        );
        self
    }

    /// A verifier no tagma can address: every enrollment read
    /// refuses (the archeion's collapsed 404).
    pub(crate) fn unenrolled() -> Self {
        Self {
            unreachable: false,
            disabled: std::sync::Mutex::new(Vec::new()),
            enrollment_wildcard: false,
            enrollment: std::collections::HashMap::new(),
            tagma_tokens: default_tagma_tokens(),
            verify_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl crate::management::AuthVerifier for MockVerifier {
    async fn verify_bearer(
        &self,
        token: &str,
    ) -> Result<
        Option<kallipai_archeion_common::principal::Principal>,
        kallipai_archeion_common::control_plane::ControlPlaneError,
    > {
        use kallipai_archeion_common::control_plane::ControlPlaneError;
        use kallipai_archeion_common::ids::TagmaId;
        use kallipai_archeion_common::principal::Principal;
        self.verify_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.unreachable {
            return Err(ControlPlaneError::Backend("mock backend down".into()));
        }
        if token == TEST_USER_BEARER {
            return Ok(Some(Principal::User(
                kallipai_archeion_common::ids::UserId::from("user-plain".to_string()),
            )));
        }
        if let Some(tagma_id) = self.tagma_tokens.get(token) {
            // The archeion's verify-bearer refuses a token whose owner
            // account is disabled, same as a revoked token: the mock
            // mirrors that collapsed refusal.
            let disabled = self.disabled.lock().expect("disabled lock");
            if disabled.contains(tagma_id) {
                return Ok(None);
            }
            return Ok(Some(Principal::Tagma(TagmaId::from(tagma_id.clone()))));
        }
        Ok((token == TEST_ADMIN_BEARER).then_some(Principal::Admin))
    }

    async fn verify_session(
        &self,
        cookie: &str,
    ) -> Result<
        Option<kallipai_archeion_common::control_plane::VerifiedSession>,
        kallipai_archeion_common::control_plane::ControlPlaneError,
    > {
        use kallipai_archeion_common::control_plane::{ControlPlaneError, VerifiedSession};
        if self.unreachable {
            return Err(ControlPlaneError::Backend("mock backend down".into()));
        }
        if cookie == TEST_USER_COOKIE {
            return Ok(Some(VerifiedSession {
                user_id: kallipai_archeion_common::ids::UserId::from("user-plain".to_string()),
                username: "alice".to_owned(),
                display_name: None,
                local_admin: false,
            }));
        }
        Ok((cookie == TEST_ADMIN_COOKIE).then(|| VerifiedSession {
            user_id: kallipai_archeion_common::ids::UserId::from("user-testadmin".to_string()),
            username: "admin".to_owned(),
            display_name: None,
            local_admin: true,
        }))
    }
    async fn enrollment_lookup(
        &self,
        tagma_id: &str,
    ) -> Result<
        Option<kallipai_archeion_common::control_plane::EnrollmentLookup>,
        kallipai_archeion_common::control_plane::ControlPlaneError,
    > {
        use kallipai_archeion_common::control_plane::ControlPlaneError;
        use kallipai_archeion_common::ids::TagmaId;
        if self.unreachable {
            return Err(ControlPlaneError::Backend("mock backend down".into()));
        }
        let owner = if self.enrollment_wildcard {
            Some("user-plain".to_owned())
        } else {
            self.enrollment.get(tagma_id).cloned()
        };
        Ok(owner.map(
            |user_id| kallipai_archeion_common::control_plane::EnrollmentLookup {
                user_id: kallipai_archeion_common::ids::UserId::from(user_id),
                enrolled_tagmas: vec![TagmaId::from(tagma_id.to_owned())],
            },
        ))
    }

    async fn search_accounts(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<
        Vec<kallipai_archeion_common::control_plane::UserIdentity>,
        kallipai_archeion_common::control_plane::ControlPlaneError,
    > {
        use kallipai_archeion_common::control_plane::{ControlPlaneError, UserIdentity};
        use kallipai_archeion_common::ids::UserId;
        if self.unreachable {
            return Err(ControlPlaneError::Backend("mock backend down".into()));
        }
        // A tiny directory already ordered by username: the id prefix, the
        // username prefix, and the email prefix all match; the email stays
        // a match key (carol is disabled and still answers, flagged).
        let directory = [
            (
                "acct-alice",
                "alice",
                Some("Alice"),
                false,
                "alice@example.test",
            ),
            ("acct-bob", "bob", None, false, "bob@example.test"),
            (
                "acct-carol",
                "carol",
                Some("Carol"),
                true,
                "carol@example.test",
            ),
        ];
        let mut hits: Vec<_> = directory
            .iter()
            .filter(|(id, name, _, _, email)| {
                id.starts_with(query) || name.starts_with(query) || email.starts_with(query)
            })
            .map(|(id, name, display, disabled, _)| UserIdentity {
                user_id: UserId::from((*id).to_string()),
                username: (*name).to_owned(),
                display_name: display.map(|d| (*d).to_owned()),
                disabled: *disabled,
            })
            .collect();
        hits.truncate(limit as usize);
        Ok(hits)
    }
}

/// The authenticator handed to every test AppState (the mock verifier
/// behind [`TEST_ADMIN_BEARER`] / [`TEST_ADMIN_COOKIE`]; its enrollment
/// answers mirror the `seed_registry` fixture owners).
pub(crate) fn test_management() -> crate::management::AdminAuth {
    crate::management::AdminAuth::Platform(std::sync::Arc::new(MockVerifier::enrolled(&[
        ("tagma-under-test", "acct-test"),
        ("tagma-bound", "acc-bound"),
        ("tagma-other", "acc-other"),
        ("user-tagma", "user-plain"),
    ])))
}

/// Process-global test Postgres: started once, the container is
/// intentionally leaked so it outlives every test. Each test carves out a
/// unique database within it.
static SHARED_PG_PORT: OnceCell<u16> = OnceCell::const_new();

/// Monotonic counter for unique per-test database names.
static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

const CONTAINER_NAME: &str = "kallipai-testcontainers-pg-model-gateway";
const DB_PREFIX: &str = "gateway_test_";

/// Cold-start errors worth a retry: an ephemeral outbound source port
/// snatching the picked host port, or a sibling test binary cold-starting
/// at the same moment winning the container create. String matching is
/// fragile, but test-only and the cheapest signal for this docker chain.
fn retryable_start_error(e: &str) -> bool {
    e.contains("already in use")
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
                        // Leak the container so it stays up for the whole
                        // test process.
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

/// A test database is owned by the process whose pid is encoded in its name
/// (`gateway_test_{pid}_{n}`). The zero-connection SQL prefilter cannot tell
/// "dead" from "just created, not yet connected", so a candidate is dropped
/// only when its owner process no longer exists (`/proc/{pid}`).
/// Unparseable names are never dropped.
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
/// earlier runs (the reusable container never drops them). Hygiene, not a
/// correctness path: on any failure the dead databases just accumulate.
/// DROP DATABASE cannot run inside a transaction block, so candidates are
/// selected first, then one autocommit DROP per database, each failure
/// swallowed.
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

/// Connect to a fresh, isolated database within the shared Postgres, without
/// running migrations (the migrator's own test drives [`crate::db::migration::Migrator`]
/// explicitly). Parallel-safe: each call carves out a database named after
/// the process and a per-process counter, and defensively drops a stale
/// same-named database first (pid reuse). Dead databases disappear with the
/// container (`docker rm -f {CONTAINER_NAME}`); no other cleanup exists by
/// design.
pub(crate) async fn raw_test_db() -> Db {
    let port = shared_pg_port().await;
    let n = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let db_name = format!("{DB_PREFIX}{}_{n}", std::process::id());
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
    Database::connect(&url).await.expect("connect to test db")
}

/// A fresh test database with the schema applied (the files harness's
/// same-named helper).
pub(crate) async fn migrated_test_db() -> Db {
    use sea_orm_migration::MigratorTrait;
    let db = raw_test_db().await;
    crate::db::migration::Migrator::up(&db, None)
        .await
        .expect("run migrations");
    db
}

/// Seed the canonical fixture registry into a migrated database:
/// - set `alpha` (profiles p1, p2, p6 in failover order) -- the
///   baseline catalog set (p6 carries the responses wire family)
/// - set `beta` (profile p3) -- the second catalog set
/// - profile p4 parked in `baseline` (a draft: in no set)
/// - set `pro-set` (profile p7) in the `catalog-pro` collection
/// - provider credentials for p1/p2/p3/p6 against `upstream_base_url`
///
/// Every row carries its collection: p1/p2/p3/p4/p6 sit in `baseline`
/// because that is where their sets live, p7 in `catalog-pro` for the
/// same reason, and each account-space profile sits in the collection
/// whose single set carries it (membership never crosses a collection).
pub(crate) async fn seed_registry(db: &Db, upstream_base_url: &str) {
    use crate::registry::{
        collection, collection_publication, group, group_member, platform_group_members,
        platform_groups, platform_publications, profile, profile_set, provider, set_member,
    };
    use sea_orm::ActiveModelTrait;
    use sea_orm::Set;

    // The collections land before anything that anchors to them: the
    // profile and set rows carry composite foreign keys into
    // `collections` now (the catalog `baseline` bundle is seeded by the
    // initial migration; `catalog-pro` and the account collections are
    // fixture rows).
    for (owner, name, description) in [
        ("system", "catalog-pro", "the pro catalog"),
        ("acc-bound", "user-col", "published to everyone"),
        ("acc-bound", "own-col", "never published"),
        ("acc-bound", "grp-col", "published to the group"),
        ("acc-other", "other-col", "never published"),
    ] {
        collection::ActiveModel {
            owner: Set(owner.to_string()),
            name: Set(name.to_string()),
            description: Set(description.to_string()),
            default_set_name: Set(None),
        }
        .insert(db)
        .await
        .expect("insert collection");
    }
    let mk_profile = |id: &str, model: &str, parked: bool, collection: &str| profile::ActiveModel {
        collection_name: Set(collection.to_string()),
        profile_id: Set(id.to_string()),
        provider_id: Set(id.to_string()),
        model: Set(model.to_string()),
        max_context_window: Set(Some(128_000)),
        effort: Set(Some("high".to_string())),
        modalities: Set(Some("[\"text\"]".to_string())),
        parked: Set(parked),
        store: Set(None),
        // The fixture registry is the platform catalog.
        owner: Set(crate::registry::CATALOG_OWNER.to_string()),
    };
    let mk_provider = |id: &str, family: &str| provider::ActiveModel {
        owner: Set(crate::registry::CATALOG_OWNER.to_string()),
        provider_id: Set(id.to_string()),
        family: Set(family.to_string()),
        base_url: Set(Some(upstream_base_url.to_string())),
        ..Default::default()
    };
    let profiles = [
        mk_profile("p1", "deepseek-chat", false, "baseline"),
        mk_profile("p2", "deepseek-researcher", false, "baseline"),
        mk_profile("p3", "gpt-x", false, "baseline"),
        mk_profile("p4", "draft-x", true, "baseline"),
        mk_profile("p6", "gpt-x-responses", false, "baseline"),
        mk_profile("p7", "gpt-pro", false, "catalog-pro"),
    ];
    let providers = [
        mk_provider("p1", "deepseek"),
        mk_provider("p2", "deepseek"),
        mk_provider("p3", "openai-compatible"),
        mk_provider("p4", "openai-compatible"),
        mk_provider("p6", "openai-responses"),
        mk_provider("p7", "openai-compatible"),
    ];
    for (p, prov) in profiles.into_iter().zip(providers) {
        // The provider row lands first: the profile's FK needs it.
        prov.insert(db).await.expect("insert provider");
        p.insert(db).await.expect("insert profile");
    }

    for (name, description) in [("alpha", "the allowed set"), ("beta", "not allowed")] {
        profile_set::ActiveModel {
            name: Set(name.to_string()),
            description: Set(description.to_string()),
            owner: Set("system".to_string()),
            collection_name: Set("baseline".to_string()),
        }
        .insert(db)
        .await
        .expect("insert set");
    }
    let mk_member =
        |collection: &str, set: &str, id: &str, position: i32| set_member::ActiveModel {
            collection_name: Set(collection.to_string()),
            set_name: Set(set.to_string()),
            profile_id: Set(id.to_string()),
            position: Set(position),
            owner: Set(crate::registry::CATALOG_OWNER.to_string()),
        };
    for m in [
        mk_member("baseline", "alpha", "p1", 0),
        mk_member("baseline", "alpha", "p2", 1),
        mk_member("baseline", "beta", "p3", 0),
        mk_member("baseline", "alpha", "p6", 2),
    ] {
        m.insert(db).await.expect("insert member");
    }

    for id in ["p1", "p2", "p3", "p7", "p6"] {
        crate::secret::provider_credential::ActiveModel {
            owner: Set(crate::registry::CATALOG_OWNER.to_string()),
            provider_id: Set(id.to_string()),
            api_key: Set(UPSTREAM_KEY.to_string()),
        }
        .insert(db)
        .await
        .expect("insert credential");
    }

    // The visibility fixture: two bound accounts with their own
    // registry spaces. acc-bound owns `user-col` (published to the
    // everyone audience, set `user-set`) and `own-col` (never published,
    // set `own-set` -- the own-collection reach clause) plus `grp-col`
    // (published to the `grp-bound` group, set `grp-set`); acc-other
    // sits in that group. The bound tagmas carry no rows -- the
    // visibility tests bind their bearers through the mock verifier's
    // enrollment map.
    let space_profile =
        |owner: &str, collection: &str, id: &str, model: &str| profile::ActiveModel {
            collection_name: Set(collection.to_string()),
            profile_id: Set(id.to_string()),
            provider_id: Set(id.to_string()),
            model: Set(model.to_string()),
            max_context_window: Set(Some(128_000)),
            effort: Set(Some("high".to_string())),
            modalities: Set(Some("[\"text\"]".to_string())),
            parked: Set(false),
            store: Set(None),
            owner: Set(owner.to_string()),
        };
    let space_provider = |owner: &str, id: &str| provider::ActiveModel {
        owner: Set(owner.to_string()),
        provider_id: Set(id.to_string()),
        family: Set("deepseek".to_string()),
        base_url: Set(Some(upstream_base_url.to_string())),
        ..Default::default()
    };
    for (owner, collection, id, model) in [
        ("acc-bound", "user-col", "u1", "bound-model"),
        ("acc-bound", "user-col", "u3", "bound-light"),
        ("acc-bound", "own-col", "u4", "bound-own"),
        ("acc-bound", "grp-col", "u5", "bound-group"),
        ("acc-other", "other-col", "u2", "other-model"),
    ] {
        space_provider(owner, id)
            .insert(db)
            .await
            .expect("insert space provider");
        crate::secret::provider_credential::ActiveModel {
            owner: Set(owner.to_string()),
            provider_id: Set(id.to_string()),
            api_key: Set(UPSTREAM_KEY.to_string()),
        }
        .insert(db)
        .await
        .expect("insert space credential");
        space_profile(owner, collection, id, model)
            .insert(db)
            .await
            .expect("insert space profile");
    }
    for (owner, collection_name, set_name, profile_id) in [
        ("acc-bound", "user-col", "user-set", "u1"),
        ("acc-bound", "own-col", "own-set", "u4"),
        ("acc-bound", "grp-col", "grp-set", "u5"),
        ("acc-other", "other-col", "other-set", "u2"),
    ] {
        // The set row carries its collection anchor directly (the
        // membership authority lives on the set row).
        profile_set::ActiveModel {
            name: Set(set_name.to_string()),
            description: Set(format!("the {set_name} fixture set")),
            owner: Set(owner.to_string()),
            collection_name: Set(collection_name.to_string()),
        }
        .insert(db)
        .await
        .expect("insert space set");
        set_member::ActiveModel {
            collection_name: Set(collection_name.to_string()),
            set_name: Set(set_name.to_string()),
            profile_id: Set(profile_id.to_string()),
            position: Set(0),
            owner: Set(owner.to_string()),
        }
        .insert(db)
        .await
        .expect("insert space member");
    }
    group::ActiveModel {
        group_id: Set("grp-bound".to_string()),
        owner: Set("acc-bound".to_string()),
        name: Set("the group".to_string()),
        created_at: Set(time::OffsetDateTime::now_utc()),
    }
    .insert(db)
    .await
    .expect("insert group");
    group_member::ActiveModel {
        group_id: Set("grp-bound".to_string()),
        member_account: Set("acc-other".to_string()),
    }
    .insert(db)
    .await
    .expect("insert group member");
    set_member::ActiveModel {
        collection_name: Set("user-col".to_string()),
        set_name: Set("user-set".to_string()),
        profile_id: Set("u3".to_string()),
        position: Set(1),
        owner: Set("acc-bound".to_string()),
    }
    .insert(db)
    .await
    .expect("insert the second user-set member");
    for publication in [
        collection_publication::ActiveModel {
            owner: Set("acc-bound".to_string()),
            collection_name: Set("user-col".to_string()),
            group_id: Set(crate::registry::EVERYONE_GROUP_ID.to_string()),
        },
        collection_publication::ActiveModel {
            owner: Set("acc-bound".to_string()),
            collection_name: Set("grp-col".to_string()),
            group_id: Set("grp-bound".to_string()),
        },
    ] {
        publication.insert(db).await.expect("insert publication");
    }
    // The platform side of the wall: a platform group with acc-other
    // in it, and a catalog collection published to that group with
    // its anchored set.
    platform_groups::ActiveModel {
        group_id: Set("pg-bound".to_string()),
        owner: Set("system".to_string()),
        name: Set("the platform group".to_string()),
        created_at: Set(time::OffsetDateTime::now_utc()),
    }
    .insert(db)
    .await
    .expect("insert platform group");
    platform_group_members::ActiveModel {
        group_id: Set("pg-bound".to_string()),
        member_account: Set("acc-other".to_string()),
    }
    .insert(db)
    .await
    .expect("insert platform group member");
    profile_set::ActiveModel {
        name: Set("pro-set".to_string()),
        description: Set("the pro-only set".to_string()),
        owner: Set("system".to_string()),
        collection_name: Set("catalog-pro".to_string()),
    }
    .insert(db)
    .await
    .expect("insert pro-set");
    set_member::ActiveModel {
        collection_name: Set("catalog-pro".to_string()),
        set_name: Set("pro-set".to_string()),
        profile_id: Set("p7".to_string()),
        position: Set(0),
        owner: Set("system".to_string()),
    }
    .insert(db)
    .await
    .expect("insert pro-set member");
    platform_publications::ActiveModel {
        owner: Set("system".to_string()),
        collection_name: Set("catalog-pro".to_string()),
        group_id: Set("pg-bound".to_string()),
    }
    .insert(db)
    .await
    .expect("insert platform publication");
}

/// The capture writer the line-reading suites share: the formatted lines
/// land in the buffer the test reads back, whichever thread wrote them.
pub(crate) struct SharedBuf(pub(crate) std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = MutexGuardWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        MutexGuardWriter(self.0.lock().unwrap())
    }
}

pub(crate) struct MutexGuardWriter<'a>(pub(crate) std::sync::MutexGuard<'a, Vec<u8>>);

impl Write for MutexGuardWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// The recorder the data-plane span assertions read: built once, beside the
/// line capture, because the process gets one global subscriber.
static RECORDER: std::sync::OnceLock<Recorder> = std::sync::OnceLock::new();

/// The spans the process exported so far, as a collector would see them:
/// names, kinds, trace and span ids, parents, status and attributes.
pub(crate) fn global_spans() -> Vec<ObservedSpan> {
    global_capture();
    RECORDER.get().map(Recorder::observed).unwrap_or_default()
}

/// The process-global capture the end-to-end line assertions read: the
/// terminal phase writes from pingora's own thread, so the thread-local
/// guard the unit tests use would miss it. Installed once for the whole
/// binary and shared -- a second caller gets the same buffer instead of a
/// panic, so the next end-to-end test calls this rather than installing a
/// subscriber of its own. Every test's lines land in the one buffer, so
/// an assertion selects its line with a needle only that test produces.
///
/// The same subscriber carries the span recorder, for the same reason: a
/// span a phase opens or a client call makes leaves through it, and the
/// span assertions read what crossed that boundary.
pub(crate) fn global_capture() -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
    static LINES: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<Vec<u8>>>> =
        std::sync::OnceLock::new();
    LINES
        .get_or_init(|| {
            let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorder = Recorder::new("model-gateway");
            // The span layer goes on first: its type is erased against the
            // bare registry, and the event layer stacks on top of it.
            let subscriber = tracing_subscriber::registry().with(recorder.layer()).with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(SharedBuf(lines.clone()))
                    .with_filter(tracing_subscriber::EnvFilter::new("warn")),
            );
            // The slot is wanted once. Losing it to a foreign subscriber
            // leaves the buffer empty and surfaces as the assertion that
            // never found its line; aborting here would take the whole
            // binary down from whichever test installed second.
            let _ = tracing::subscriber::set_global_default(subscriber);
            let _ = RECORDER.set(recorder);
            lines
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_start_error_matches_both_cold_start_conflicts() {
        assert!(retryable_start_error(
            "Bind for 127.0.0.1:5432 failed: port is already allocated: address already in use",
        ));
        assert!(retryable_start_error(
            "Conflict. The container name \"kallipai-testcontainers-pg-model-gateway\" is already in use by container 4f0c",
        ));
        assert!(!retryable_start_error("pull access denied"));
    }

    #[test]
    fn owner_dead_requires_a_dead_encoded_pid() {
        // This process is alive, so its own databases are never swept.
        assert!(!owner_dead(&format!("{DB_PREFIX}{}_0", std::process::id())));
        // 4e9 is far beyond Linux pid_max: no such process exists.
        assert!(owner_dead(&format!("{DB_PREFIX}4000000000_0")));
        assert!(!owner_dead("gateway_test_4000000000"));
        assert!(!owner_dead("files_test_4000000000_0"));
    }
}
