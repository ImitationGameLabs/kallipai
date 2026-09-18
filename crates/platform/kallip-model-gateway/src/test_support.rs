//! Test-only fixtures: an ephemeral per-process Postgres (testcontainers)
//! with one isolated database per test. Ported from the files harness; this
//! crate talks to its own container so gateway tests never contend with the
//! other services' containers. Needs Docker at test time.

use std::sync::atomic::{AtomicU64, Ordering};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ImageExt, ReuseDirective};
use tokio::sync::OnceCell;

use crate::db::Db;

/// The canonical test bearer; seeded with allowed set `alpha`.
pub(crate) const TEST_BEARER: &str = "test-proxy-key-0123456789abcdef";
/// A bearer whose stored key has no allowed sets -- the zero-authorization
/// edge exercised by the default-path fail-open regression.
pub(crate) const TEST_BEARER_STARVED: &str = "test-proxy-key-no-sets-0123456789ab";

/// The upstream provider key seeded behind profile p1/p2/p3 credentials.
pub(crate) const UPSTREAM_KEY: &str = "sk-upstream-secret";

/// The management credential every test router is built with (the
/// management face's static token).
pub(crate) const TEST_MANAGEMENT_TOKEN: &str = "test-management-token";

/// The authenticator handed to every test AppState (the static
/// token behind [`TEST_MANAGEMENT_TOKEN`]).
pub(crate) fn test_management() -> std::sync::Arc<dyn crate::management::ManagementAuth> {
    std::sync::Arc::new(crate::management::StaticManagementToken::new(
        TEST_MANAGEMENT_TOKEN,
    ))
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
/// - set `alpha` (profiles p1, p2 in failover order) -- the key's allowed set
/// - set `beta` (profile p3) -- exists but NOT allowed for the key
/// - profile p4 parked (the parking resource)
/// - upstream credentials for p1/p2/p3 against `upstream_base_url`
/// - registry default marker = `alpha`
/// - the proxy key for [`TEST_BEARER`], allowed only on `alpha`
/// - a starved second key ([`TEST_BEARER_STARVED`]) with no allowed sets
pub(crate) async fn seed_registry(db: &Db, upstream_base_url: &str) {
    use crate::registry::{profile, profile_set, registry_meta, set_member};
    use crate::secret::tagma;
    use crate::secret::{proxy_key, proxy_key_set};
    use sea_orm::ActiveModelTrait;
    use sea_orm::Set;

    let mk_profile = |id: &str, family: &str, model: &str, parked: bool| profile::ActiveModel {
        profile_id: Set(id.to_string()),
        family: Set(family.to_string()),
        model: Set(model.to_string()),
        max_context_window: Set(Some(128_000)),
        effort: Set(Some("high".to_string())),
        modalities: Set(Some("[\"text\"]".to_string())),
        parked: Set(parked),
        max_budget: Set(None),
        tpm_limit: Set(None),
        rpm_limit: Set(None),
    };
    for p in [
        mk_profile("p1", "deepseek", "deepseek-chat", false),
        mk_profile("p2", "deepseek", "deepseek-researcher", false),
        mk_profile("p3", "openai-compatible", "gpt-x", false),
        mk_profile("p4", "openai-compatible", "draft-x", true),
    ] {
        p.insert(db).await.expect("insert profile");
    }

    for (name, description) in [("alpha", "the allowed set"), ("beta", "not allowed")] {
        profile_set::ActiveModel {
            name: Set(name.to_string()),
            description: Set(description.to_string()),
        }
        .insert(db)
        .await
        .expect("insert set");
    }
    let mk_member = |set: &str, id: &str, position: i32| set_member::ActiveModel {
        set_name: Set(set.to_string()),
        profile_id: Set(id.to_string()),
        position: Set(position),
    };
    for m in [
        mk_member("alpha", "p1", 0),
        mk_member("alpha", "p2", 1),
        mk_member("beta", "p3", 0),
    ] {
        m.insert(db).await.expect("insert member");
    }

    registry_meta::ActiveModel {
        key: Set(crate::registry::DEFAULT_SET_KEY.to_string()),
        value: Set("alpha".to_string()),
    }
    .insert(db)
    .await
    .expect("insert default marker");

    for id in ["p1", "p2", "p3"] {
        crate::secret::upstream_credential::ActiveModel {
            profile_id: Set(id.to_string()),
            upstream_base_url: Set(upstream_base_url.to_string()),
            upstream_api_key: Set(UPSTREAM_KEY.to_string()),
        }
        .insert(db)
        .await
        .expect("insert credential");
    }

    // The identity row behind every seeded key (the FK target); unlimited
    // quotas by default -- the quota tests tighten it with an UPDATE.
    tagma::ActiveModel {
        tagma_id: Set("tagma-under-test".to_string()),
        account_id: Set(None),
        max_budget: Set(None),
        tpm_limit: Set(None),
        rpm_limit: Set(None),
    }
    .insert(db)
    .await
    .expect("insert tagma");

    let key_hash = kallip_common::authtoken::TokenHash::of(TEST_BEARER)
        .as_bytes()
        .to_vec();
    proxy_key::ActiveModel {
        key_hash: Set(key_hash.clone()),
        tagma_id: Set("tagma-under-test".to_string()),
        created_at: Set(time::OffsetDateTime::now_utc()),
        expires_at: Set(None),
    }
    .insert(db)
    .await
    .expect("insert key");
    proxy_key_set::ActiveModel {
        key_hash: Set(key_hash),
        set_name: Set("alpha".to_string()),
    }
    .insert(db)
    .await
    .expect("insert key set");
    let starved_hash = kallip_common::authtoken::TokenHash::of(TEST_BEARER_STARVED)
        .as_bytes()
        .to_vec();
    proxy_key::ActiveModel {
        key_hash: Set(starved_hash),
        tagma_id: Set("tagma-under-test".to_string()),
        created_at: Set(time::OffsetDateTime::now_utc()),
        expires_at: Set(None),
    }
    .insert(db)
    .await
    .expect("insert starved key");
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
