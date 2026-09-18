//! The gateway durable store (sea-orm / Postgres): the connection
//! pool, the migration registry, and the store-side plumbing the
//! business entities -- the profile registry and the credential
//! tables -- hang off.

pub mod migration;

use anyhow::Result;
use sea_orm::{Database, DatabaseConnection, DbErr};
use sea_orm_migration::MigratorTrait;
use tracing::info;

/// A cloned handle to the durable store. Cheap to clone (one shared pool).
pub type Db = DatabaseConnection;

/// Connect to Postgres and apply all pending gateway migrations.
pub async fn connect_and_migrate(url: &str) -> Result<Db> {
    let db = Database::connect(url).await?;
    migration::Migrator::up(&db, None).await?;
    info!("kallip-model-gateway connected to Postgres");
    Ok(db)
}

/// Map a sea-orm [`DbErr`] to an HTTP 500. A DB failure is a transient
/// server-side fault, never a client error. (A `From<DbErr> for ApiError`
/// impl would violate the orphan rule -- both types are foreign -- so
/// handlers map explicitly, the lesche shape.)
pub fn map_db_err(e: DbErr) -> kallip_common::protocol::ApiError {
    kallip_common::protocol::ApiError::internal(format_args!("database error: {e}"))
}
