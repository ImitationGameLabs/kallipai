//! Durable control-plane store (sea-orm / Postgres).
//!
//! Holds the identity / credentials / provisioning layer: users, passkeys,
//! invite codes, enrollment tokens, tagmata, tagma tokens, sessions, and WebAuthn
//! challenges. The soft-state data plane (presence, routing, dedup, key-exchange
//! correlation) stays in the bin's in-memory `Registry` — see
//! `.draft/design/relay-service.md` for the durable/soft-state boundary.
//!
//! The migrations under [`migration`] prime the full schema.

pub mod entity;
pub mod migration;

use std::time::Duration;

use anyhow::Result;
use kallip_common::protocol::ApiError;
use sea_orm::{
    ColumnTrait, Database, DatabaseConnection, DbErr, EntityTrait, QueryFilter, TransactionError,
};
use sea_orm_migration::MigratorTrait;
use tracing::{info, warn};

/// A cloned handle to the durable store. `DatabaseConnection` is internally
/// `Arc`'d, so cloning is cheap and shares one connection pool.
pub type Db = DatabaseConnection;

/// Map a sea-orm [`DbErr`] to an HTTP 500. Handlers propagate this via `?` after
/// `.await`ing a query; a DB failure is a transient server-side fault, never a
/// client error. (A `From<DbErr> for ApiError` impl would violate the orphan
/// rule — both types are foreign — so each call site maps explicitly.)
pub fn map_db_err(e: DbErr) -> ApiError {
    ApiError::internal(format_args!("database error: {e}"))
}

/// Unified transaction-closure error: either a DB failure or a business-rule
/// rejection surfaced as an [`ApiError`]. Used by every
/// `db.transaction::<_, _, TxnError>` closure so the handlers share one flatten
/// helper ([`flatten_txn`]) instead of re-rolling an identical enum per route.
/// `From<DbErr>` lets `?` convert query errors inside the closure.
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

/// Delete expired WebAuthn ceremonies. Best-effort: a failure is logged and
/// swallowed so a transient DB hiccup cannot abort the sweep (the expired rows
/// just linger until the next sweep). Driven by the background sweep task in
/// `main.rs` (every ~60s), not from the request path, so the `DELETE` does not
/// add latency to a ceremony begin.
pub(crate) async fn gc_expired_challenges(db: &Db) {
    let now = time::OffsetDateTime::now_utc();
    if let Err(e) = entity::webauthn_challenges::Entity::delete_many()
        .filter(entity::webauthn_challenges::Column::ExpiresAt.lt(now))
        .exec(db)
        .await
    {
        warn!(error = %e, "expired-challenge GC failed (non-fatal)");
    }
}

/// Connect to Postgres (retrying with a capped backoff, since the agora may boot
/// before its DB in a composed deploy) and apply all pending migrations.
pub async fn connect_and_migrate(url: &str) -> Result<Db> {
    let db = connect(url).await?;
    migration::Migrator::up(&db, None).await?;
    Ok(db)
}

/// Connect to Postgres, retrying with a capped, jittered exponential backoff
/// until the DB is reachable. The jitter de-synchronizes replicas booting
/// together against a recovering DB; the cap bounds each sleep.
async fn connect(url: &str) -> Result<Db> {
    let mut delay = Duration::from_secs(1);
    loop {
        match Database::connect(url).await {
            Ok(db) => {
                info!("connected to Postgres");
                return Ok(db);
            }
            Err(e) => {
                let jitter = jitter_up_to(delay / 2);
                warn!(
                    error = %e,
                    retry_in = ?delay,
                    jitter = ?jitter,
                    "Postgres connection failed; retrying"
                );
                tokio::time::sleep(delay + jitter).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// A random `Duration` in `[0, bound]`, drawn from the OS RNG. Returns zero on
/// RNG failure (a jitter miss is harmless; the backoff still applies).
fn jitter_up_to(bound: Duration) -> Duration {
    if bound.is_zero() {
        return Duration::ZERO;
    }
    let max_ms = u64::try_from(bound.as_millis()).unwrap_or(0);
    if max_ms == 0 {
        return Duration::ZERO;
    }
    // Map a u32 into [0, max_ms] without bias: scale by max_ms / u32::MAX.
    let r = getrandom::u32().unwrap_or(0);
    let ms = (r as u64 * max_ms) / (u32::MAX as u64);
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use sea_orm::ActiveValue::Set;
    use sea_orm::{ActiveModelTrait, EntityTrait};
    use testcontainers_modules::postgres::Postgres;
    use testcontainers_modules::testcontainers::runners::AsyncRunner;

    use super::*;

    // The default testcontainers Postgres image uses superuser `postgres` with
    // password `postgres` and database `postgres`.
    const PG_USER: &str = "postgres";
    const PG_PASSWORD: &str = "postgres";
    const PG_DB: &str = "postgres";

    /// Provision one ephemeral Postgres, connect+migrate, then round-trip a
    /// `tagmata` row (after seeding its owning `users` row, required by the
    /// `tagmata.owner_user_id -> users(id)` FK). Proves the substrate end-to-end
    /// (migrations apply, the entity/BYTEA/TIMESTAMPTZ mapping works, the
    /// connection is usable, the FK graph is sound). Needs Docker at test time.
    #[tokio::test]
    async fn connect_migrate_and_roundtrip_tagma() {
        let image = Postgres::default()
            .with_db_name(PG_DB)
            .with_user(PG_USER)
            .with_password(PG_PASSWORD);
        let container = image.start().await.expect("start postgres");
        let port = container.get_host_port_ipv4(5432).await.expect("host port");
        let url = format!("postgres://{PG_USER}:{PG_PASSWORD}@127.0.0.1:{port}/{PG_DB}");

        let db = connect_and_migrate(&url).await.expect("connect + migrate");

        let created_at = time::OffsetDateTime::now_utc();
        // The `tagmata.owner_user_id -> users(id)` FK requires the owner to
        // exist first.
        entity::users::ActiveModel {
            id: Set("owner-1".to_string()),
            username: Set("owner-1".to_string()),
            email: Set("owner-1@example.test".to_string()),
            display_name: Set(None),
            created_at: Set(created_at),
            disabled_at: Set(None),
        }
        .insert(&db)
        .await
        .expect("insert owner user");

        let inserted = entity::tagmata::ActiveModel {
            id: Set("tagma-1".to_string()),
            owner_user_id: Set("owner-1".to_string()),
            pinned_public_key: Set(vec![0u8; 32]),
            created_at: Set(created_at),
            label: Set(None),
            last_tunnel_proof_ts: Set(None),
        }
        .insert(&db)
        .await
        .expect("insert tagma");

        let found = entity::tagmata::Entity::find_by_id("tagma-1".to_string())
            .one(&db)
            .await
            .expect("find tagma")
            .expect("row present");
        assert_eq!(found.id, "tagma-1");
        assert_eq!(found.owner_user_id, "owner-1");
        assert_eq!(found.pinned_public_key, vec![0u8; 32]);
        assert_eq!(
            found.created_at.unix_timestamp(),
            created_at.unix_timestamp()
        );
        assert!(found.label.is_none());
        // The insert returns the stored model.
        assert_eq!(inserted.id, found.id);
    }
}
