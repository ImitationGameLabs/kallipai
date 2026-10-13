//! Metadata storage for kallipai-files: the Postgres schema
//! ([`crate::migration`]), the entities ([`models`]), and the reference-counted
//! write transactions ([`repo`]).

pub mod models;
pub mod repo;

use std::time::Duration;

use sea_orm::{Database, DatabaseConnection, DbErr};
use sea_orm_migration::MigratorTrait;
use tracing::{info, warn};

use crate::migration::Migrator;

/// Environment variable holding the metadata store connection string
/// (`postgres://user:password@host:port/db`).
pub const DATABASE_URL_ENV: &str = "KALLIPAI_FILES_DATABASE_URL";

/// Handle to the metadata database.
pub type Db = DatabaseConnection;

/// Connect to the metadata database and apply pending migrations. `sql_slow`
/// is the statement duration above which a query is logged at WARN; the
/// connect options are built by `kallipai_db_helper::connect_options`.
pub async fn connect_and_migrate(url: &str, sql_slow: Duration) -> Result<Db, DbErr> {
    let db = connect(url, sql_slow).await?;
    Migrator::up(&db, None).await?;
    Ok(db)
}

/// Connect to Postgres, retrying with a capped backoff: the file service may
/// boot before its database in a composed deploy. Same policy as the archeion
/// (which additionally jitters to de-synchronize replicas; the file service
/// is a single instance, so the plain cap suffices).
async fn connect(url: &str, sql_slow: Duration) -> Result<Db, DbErr> {
    let mut delay = Duration::from_secs(1);
    loop {
        match Database::connect(kallipai_db_helper::connect_options(url, sql_slow)).await {
            Ok(db) => {
                info!(
                    sql_slow_ms = sql_slow.as_millis(),
                    "connected to metadata database; SQL log tiers: every statement at debug, slow statements at warn, a storage failure at error"
                );
                return Ok(db);
            }
            Err(e) => {
                warn!(
                    error = %e,
                    retry_in = ?delay,
                    "metadata database connection failed; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Map a metadata-store failure onto the HTTP face: the failure is logged
/// at error with the database context, then surfaced as an internal error.
pub fn map_db_err(e: DbErr) -> kallipai_common::protocol::ApiError {
    tracing::error!(error = %e, "storage failure: metadata database operation failed");
    kallipai_common::protocol::ApiError::internal(format_args!("database error: {e}"))
}
