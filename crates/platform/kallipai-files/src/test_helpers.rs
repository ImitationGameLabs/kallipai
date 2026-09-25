//! Test-only fixtures: an ephemeral per-process Postgres (testcontainers)
//! with one isolated database per test. Ported from the archeion harness; this
//! crate talks to its own container so files tests never contend with the
//! archeion/lesche shared one. Needs Docker at test time.

use std::sync::atomic::{AtomicU64, Ordering};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ImageExt, ReuseDirective};
use tokio::sync::OnceCell;

use crate::metadata::Db;
use crate::migration::Migrator;

/// Process-global test Postgres: started once, the container is
/// intentionally leaked so it outlives every test. Each test carves out a
/// unique database within it.
static SHARED_PG_PORT: OnceCell<u16> = OnceCell::const_new();

/// Monotonic counter for unique per-test database names.
static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

const CONTAINER_NAME: &str = "kallipai-testcontainers-pg-files";
const DB_PREFIX: &str = "files_test_";

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
/// (`files_test_{pid}_{n}`). The zero-connection SQL prefilter cannot tell
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
/// running migrations (the migrator's own tests drive [`Migrator`]
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

/// A fresh test database with the schema applied.
pub(crate) async fn migrated_test_db() -> Db {
    let db = raw_test_db().await;
    Migrator::up(&db, None).await.expect("run migrations");
    db
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
            "Conflict. The container name \"kallipai-testcontainers-pg-files\" is already in use by container 4f0c",
        ));
        assert!(!retryable_start_error("pull access denied"));
    }

    #[test]
    fn owner_dead_requires_a_dead_encoded_pid() {
        // This process is alive, so its own databases are never swept.
        assert!(!owner_dead(&format!("{DB_PREFIX}{}_0", std::process::id())));
        // 4e9 is far beyond Linux pid_max: no such process exists.
        assert!(owner_dead(&format!("{DB_PREFIX}4000000000_0")));
        assert!(!owner_dead("files_test_4000000000"));
        assert!(!owner_dead("archeion_test_4000000000_0"));
    }
}
