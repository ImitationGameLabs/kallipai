//! The test-Postgres plumbing for the CLI's file e2e: one reusable
//! container shared with the kallipai-files integration suite (the same
//! container name and db-name prefix, so whichever test binary starts it,
//! everyone reuses it and every sweep cleans everyone's dead databases --
//! ownership is encoded in the name via the creating pid). Needs Docker
//! at test time.

use std::sync::atomic::{AtomicU64, Ordering};

use sea_orm::{ConnectionTrait, Database, DatabaseBackend, Statement};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ImageExt, ReuseDirective};
use tokio::sync::OnceCell;

static SHARED_PG_PORT: OnceCell<u16> = OnceCell::const_new();
static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

// Deliberately identical to crates/kallipai-files/tests/common: one shared
// container, one shared dead-db sweep domain.
const CONTAINER_NAME: &str = "kallipai-testcontainers-pg-files-api";
const DB_PREFIX: &str = "files_api_test_";

fn retryable_start_error(e: &str) -> bool {
    e.contains("already in use")
}

/// A test database is owned by the process whose pid is encoded in its
/// name (`{DB_PREFIX}{pid}_{n}`); a candidate is swept only when its owner
/// process is gone. Unparseable names are never dropped.
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

/// Best-effort, once-per-process cleanup of databases left dead by earlier
/// runs (the reusable container never drops them). Same hygiene contract
/// as the kallipai-files suite's sweep; DROP DATABASE cannot run inside a
/// transaction block, so candidates are selected first, then one
/// autocommit DROP each, failures swallowed.
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
/// migration -- the e2e drives `connect_and_migrate`, the service's real
/// boot path.
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
