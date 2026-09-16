//! Metadata storage for kallip-files: the Postgres schema
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
pub const DATABASE_URL_ENV: &str = "KALLIP_FILES_DATABASE_URL";

/// Handle to the metadata database.
pub type Db = DatabaseConnection;

/// Connect to the metadata database and apply pending migrations.
pub async fn connect_and_migrate(url: &str) -> Result<Db, DbErr> {
    let db = connect(url).await?;
    Migrator::up(&db, None).await?;
    Ok(db)
}

/// Connect to Postgres, retrying with a capped backoff: the file service may
/// boot before its database in a composed deploy. Same policy as the archeion
/// (which additionally jitters to de-synchronize replicas; the file service
/// is a single instance, so the plain cap suffices).
async fn connect(url: &str) -> Result<Db, DbErr> {
    let mut delay = Duration::from_secs(1);
    loop {
        match Database::connect(url).await {
            Ok(db) => {
                info!("connected to metadata database");
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
