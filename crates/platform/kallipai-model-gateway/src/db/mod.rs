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
    info!("kallipai-model-gateway connected to Postgres");
    Ok(db)
}

/// Map a sea-orm [`DbErr`] to an HTTP error. A storage-level unique
/// violation is a client-side conflict -- two writers raced one globally
/// unique name or id -- and every other DB failure stays a transient
/// server-side fault. (A `From<DbErr> for ApiError` impl would violate
/// the orphan rule -- both types are foreign -- so handlers map
/// explicitly, the lesche shape.)
pub fn map_db_err(e: DbErr) -> kallipai_common::protocol::ApiError {
    if is_unique_violation(&e) {
        return kallipai_common::protocol::ApiError::conflict(
            pg_unique_detail(&e).unwrap_or("a row with that identity already exists"),
        );
    }
    kallipai_common::protocol::ApiError::internal(format_args!("database error: {e}"))
}

/// Whether the error is a Postgres unique violation (SQLSTATE 23505),
/// unwrapped from the sea-orm runtime layers down to the sqlx database
/// error that carries the state code.
pub(crate) fn is_unique_violation(e: &DbErr) -> bool {
    let runtime = match e {
        DbErr::Exec(runtime) | DbErr::Query(runtime) | DbErr::Conn(runtime) => runtime,
        _ => return false,
    };
    let sqlx_err = match runtime {
        sea_orm::RuntimeErr::SqlxError(err) => err,
        sea_orm::RuntimeErr::Internal(_) => return false,
    };
    matches!(sqlx_err, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

/// The Postgres unique-violation detail names the key tuple that
/// collided (the entity id). The unwrap in [`map_db_err`] falls
/// back to the generic text when the driver shape hides it.
fn pg_unique_detail(e: &DbErr) -> Option<&str> {
    let runtime = match e {
        DbErr::Exec(runtime) | DbErr::Query(runtime) | DbErr::Conn(runtime) => runtime,
        _ => return None,
    };
    let sqlx_err = match runtime {
        sea_orm::RuntimeErr::SqlxError(err) => err,
        _ => return None,
    };
    match sqlx_err {
        sqlx::Error::Database(db) => db
            .downcast_ref::<sqlx::postgres::PgDatabaseError>()
            .detail(),
        _ => None,
    }
}
