//! The lesche durable store (sea-orm / Postgres).
//!
//! The data-plane relay persists one durable surface -- the plaintext
//! room-message history of each room (the `room_messages` entity in [`store`]);
//! everything else (presence, routing, KEX correlation) is in-memory soft-state
//! in the bin's `Registry`, rebuilt on restart. The store is `Option<Db>` on
//! the relay state: `Some` in production, `None` in the mock-state routing
//! tests.

pub mod entity;
pub mod migration;
pub mod store;

use anyhow::Result;
use kallip_common::protocol::ApiError;
use sea_orm::{Database, DatabaseConnection, DbErr, TransactionError};
use sea_orm_migration::MigratorTrait;
use tracing::info;

/// A cloned handle to the durable store. Cheap to clone (one shared pool).
pub type Db = DatabaseConnection;

/// Map a sea-orm [`DbErr`] to an HTTP 500. A DB failure is a transient
/// server-side fault, never a client error. (A `From<DbErr> for ApiError` impl
/// would violate the orphan rule -- both types are foreign -- so each call site
/// maps explicitly.)
pub fn map_db_err(e: DbErr) -> ApiError {
    ApiError::internal(format_args!("database error: {e}"))
}

/// Unified transaction-closure error: either a DB failure or a business-rule
/// rejection surfaced as an [`ApiError`]. Used by every
/// `db.transaction::<_, _, TxnError>` closure so the handlers share one flatten
/// helper ([`flatten_txn`]). `From<DbErr>` lets `?` convert query errors inside
/// the closure.
#[derive(Debug)]
pub enum TxnError {
    Db(DbErr),
    Api(ApiError),
}

impl std::fmt::Display for TxnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxnError::Db(e) => write!(f, "db: {e}"),
            TxnError::Api(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TxnError {}

impl From<DbErr> for TxnError {
    fn from(e: DbErr) -> Self {
        TxnError::Db(e)
    }
}

/// Flatten a `TransactionResult<T, TxnError>` into `Result<T, ApiError>`:
/// business-rule rejections surface as themselves, every DB-flavored branch
/// (the closure's `Db` or a connection-level failure) maps through
/// [`map_db_err`]. Call sites that need to discriminate a specific DB error
/// (e.g. a unique-constraint violation) handle the `TransactionError` directly
/// before falling back to this.
pub fn flatten_txn<T>(r: Result<T, TransactionError<TxnError>>) -> Result<T, ApiError> {
    match r {
        Ok(t) => Ok(t),
        Err(TransactionError::Transaction(TxnError::Api(e))) => Err(e),
        Err(TransactionError::Transaction(TxnError::Db(e)))
        | Err(TransactionError::Connection(e)) => Err(map_db_err(e)),
    }
}

/// Connect to Postgres and apply all pending lesche migrations.
pub async fn connect_and_migrate(url: &str) -> Result<Db> {
    let db = Database::connect(url).await?;
    migration::Migrator::up(&db, None).await?;
    info!("lesche connected to Postgres");
    Ok(db)
}
