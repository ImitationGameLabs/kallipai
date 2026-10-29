//! The gateway durable store (sea-orm / Postgres): the connection
//! pool, the migration registry, and the store-side plumbing the
//! business entities -- the profile registry and the credential
//! tables -- hang off.

pub mod migration;

use std::time::Duration;

use anyhow::Result;
use sea_orm::{Database, DatabaseConnection, DbErr};
use sea_orm_migration::MigratorTrait;
use tracing::{error, info};

/// A cloned handle to the durable store. Cheap to clone (one shared pool).
pub type Db = DatabaseConnection;

/// Connect to Postgres and apply all pending gateway migrations.
///
/// `sql_slow` is the statement duration above which a query is logged at
/// WARN; the connect options are built by kallipai_db_helper::connect_options.
pub async fn connect_and_migrate(url: &str, sql_slow: Duration) -> Result<Db> {
    let db = Database::connect(kallipai_db_helper::connect_options(url, sql_slow)).await?;
    migration::Migrator::up(&db, None).await?;
    info!(
        sql_slow_ms = sql_slow.as_millis(),
        "kallipai-model-gateway connected to Postgres; SQL log tiers: every statement at debug, slow statements at warn, a storage failure at error"
    );
    Ok(db)
}

/// Map a sea-orm [`DbErr`] to an HTTP error. A storage-level unique
/// violation is a client-side conflict -- two writers raced one globally
/// unique name or id -- and every other DB failure stays a transient
/// server-side fault, logged at ERROR here: this is the single mapping
/// point the crate's handlers route a storage failure through, so the
/// tier is stated once rather than per call site. (A `From<DbErr> for
/// ApiError` impl would violate the orphan rule -- both types are
/// foreign -- so handlers map explicitly, the lesche shape.)
///
/// One store failure on the selection path writes three ERROR records:
/// this mapping's own line (below), the record `ApiError::internal`
/// writes when it takes the same text on the common side, and the
/// gateway's terminal failure line, which names the request, the
/// `store_error` reason and the same sentence. Three records for one
/// event is the deliberate price of that failure line carrying its own
/// attribution; the common-side record stays untouched, since muting it
/// would change a surface other services share.
pub fn map_db_err(e: DbErr) -> kallipai_common::protocol::ApiError {
    if is_unique_violation(&e) {
        return kallipai_common::protocol::ApiError::conflict(
            pg_unique_detail(&e).unwrap_or("a row with that identity already exists"),
        );
    }
    if is_foreign_key_violation(&e) {
        // A 23503 trip stays a client conflict, so the answer names the
        // rule rather than the constraint the store reported. Two arms
        // name their rule. The provider arm covers both directions -- a
        // provider missing from the profile's space, and a delete the
        // store blocks because a profile still names it (ON DELETE
        // RESTRICT). The set arm covers a write naming a set that is not
        // there, which includes the set a concurrent delete removed.
        // Scoping follows the key: a provider row is keyed by owner and
        // id, so a profile's provider only has to share its space, while
        // a set is keyed by collection too, so a profile's set must share
        // both. A trip there is a caller mistake the handlers check for
        // first. Every other trip -- a reference pointing at a row the
        // store does not hold, or a constraint this build does not name --
        // lands in the fallback sentence below, which says where the
        // reference points instead of guessing at the rule.
        let message = match pg_constraint_name(&e) {
            Some("fk_profiles_provider") => {
                "a provider must exist in the profile's space and cannot be deleted while a profile names it"
            }
            Some("fk_profiles_set") => {
                "a profile's set must exist in its own collection, and a set deleted while that write was in flight answers the same way"
            }
            _ => {
                "the reference names a row that is not there, or a constraint this build does not name"
            }
        };
        return kallipai_common::protocol::ApiError::conflict(message);
    }
    error!(error = %e, "database failure: mapping to an internal error response");
    kallipai_common::protocol::ApiError::internal(store_error_detail(&e))
}

/// The sentence one store failure quotes: the store mapping hands it to
/// the internal-error constructor, and the terminal failure line records
/// the same text, so the two log records read as one event.
pub fn store_error_detail(e: &DbErr) -> String {
    format!("database error: {e}")
}

/// The driver's own database error, unwrapped from the sea-orm runtime
/// layers; `None` for a shape that hides it (an internal error, or a
/// driver carrying no database error).
fn pg_database_error(e: &DbErr) -> Option<&(dyn sqlx::error::DatabaseError + 'static)> {
    let runtime = match e {
        DbErr::Exec(runtime) | DbErr::Query(runtime) | DbErr::Conn(runtime) => runtime,
        _ => return None,
    };
    match runtime {
        sea_orm::RuntimeErr::SqlxError(sqlx::Error::Database(db)) => Some(db.as_ref()),
        _ => None,
    }
}

/// The SQLSTATE the driver reported, when its shape carries one.
fn sqlstate(e: &DbErr) -> Option<std::borrow::Cow<'_, str>> {
    pg_database_error(e).and_then(|db| db.code())
}

/// Whether the error is a Postgres unique violation (SQLSTATE 23505).
pub(crate) fn is_unique_violation(e: &DbErr) -> bool {
    sqlstate(e).as_deref() == Some("23505")
}

/// Whether the error is a Postgres foreign-key violation (SQLSTATE 23503).
/// [`map_db_err`] answers it as a client conflict: two arms name their rule
/// (the provider reference and the profile's set reference), and every other
/// trip falls to the fallback sentence.
pub(crate) fn is_foreign_key_violation(e: &DbErr) -> bool {
    sqlstate(e).as_deref() == Some("23503")
}

/// The constraint a Postgres foreign-key violation tripped, when the
/// driver shape carries it: the fact that tells the two named arms apart.
fn pg_constraint_name(e: &DbErr) -> Option<&str> {
    pg_database_error(e)
        .and_then(|db| db.try_downcast_ref::<sqlx::postgres::PgDatabaseError>())
        .and_then(|pg| pg.constraint())
}

/// The Postgres unique-violation detail names the key tuple that
/// collided (the entity id). The unwrap in [`map_db_err`] falls
/// back to the generic text when the driver shape hides it -- the
/// downcast is the fallible one, so a database error from another
/// driver shape answers `None` instead of panicking.
fn pg_unique_detail(e: &DbErr) -> Option<&str> {
    pg_database_error(e)
        .and_then(|db| db.try_downcast_ref::<sqlx::postgres::PgDatabaseError>())
        .and_then(|pg| pg.detail())
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use sea_orm::{DbErr, RuntimeErr};
    use tracing_subscriber::prelude::*;

    use super::map_db_err;

    /// A stand-in for the driver's database error: the tier branch reads
    /// the SQLSTATE, and the Postgres detail is absent from a shape that
    /// is not the Postgres driver's own.
    #[derive(Debug)]
    struct DriverError(&'static str);

    impl std::fmt::Display for DriverError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "driver error {}", self.0)
        }
    }

    impl std::error::Error for DriverError {}

    impl sqlx::error::DatabaseError for DriverError {
        fn message(&self) -> &str {
            "duplicate key value violates unique constraint"
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.0))
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::UniqueViolation
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
    }

    /// The sea-orm shape a failed statement arrives in.
    fn db_error(state: &'static str) -> DbErr {
        DbErr::Query(RuntimeErr::SqlxError(sqlx::Error::Database(Box::new(
            DriverError(state),
        ))))
    }

    /// An in-memory sink for the events a call emits, so the assertions
    /// read the emitted stream rather than the branch behind it.
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = GuardWriter<'a>;

        fn make_writer(&'a self) -> Self::Writer {
            GuardWriter(self.0.lock().unwrap())
        }
    }

    struct GuardWriter<'a>(std::sync::MutexGuard<'a, Vec<u8>>);

    impl Write for GuardWriter<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    /// The connect options carry the three tiers: statement logging on,
    /// every statement at DEBUG (quiet under the default `info` filter),
    /// a WARN above the caller's threshold -- which arrives as an
    /// argument rather than as a default baked in here.
    #[test]
    fn connect_options_set_the_sql_log_tiers() {
        for threshold_ms in [100_u64, 2500] {
            let slow = Duration::from_millis(threshold_ms);
            let options = kallipai_db_helper::connect_options("postgres://user:pass@host/db", slow);
            assert!(options.get_sqlx_logging());
            assert_eq!(options.get_sqlx_logging_level(), log::LevelFilter::Debug);
            assert_eq!(
                options.get_sqlx_slow_statements_logging_settings(),
                (log::LevelFilter::Warn, slow)
            );
        }
    }

    /// The failure tier: a unique violation is the client's own conflict
    /// (409) and stays out of the ERROR stream; every other storage
    /// failure is a 500 and writes one ERROR event.
    #[test]
    fn map_db_err_logs_every_failure_but_the_unique_violation() {
        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(SharedBuf(captured.clone()))
                .with_filter(tracing_subscriber::EnvFilter::new("error")),
        );
        let _guard = tracing::subscriber::set_default(subscriber);

        let conflict = map_db_err(db_error("23505"));
        assert_eq!(conflict.status, 409);
        assert!(
            captured.lock().unwrap().is_empty(),
            "a unique violation is a client conflict, not an ERROR"
        );

        let internal = map_db_err(db_error("08006"));
        assert_eq!(internal.status, 500);
        let logged =
            String::from_utf8(captured.lock().unwrap().clone()).expect("log bytes are utf8");
        assert!(
            logged.contains("database failure"),
            "the storage failure must log at ERROR: {logged}"
        );
    }

    /// The provider arm of the 23503 split on a real driver error: the
    /// handlers count riders before deleting a provider, so only a direct
    /// delete reaches the constraint -- the shape a caller racing another
    /// delete sees, and the answer names the reference, not a collection.
    #[tokio::test]
    async fn a_delete_blocked_by_a_live_profile_names_the_provider_reference() {
        use sea_orm::ConnectionTrait;

        let db = crate::test_support::migrated_test_db().await;
        db.execute_unprepared(
            "INSERT INTO providers (owner, provider_id, family) VALUES ('system', 'p', 'deepseek')",
        )
        .await
        .expect("provider insert");
        db.execute_unprepared(
            "INSERT INTO profiles (owner, collection_name, profile_id, provider_id, model) VALUES ('system', 'baseline', 'p9', 'p', 'm')",
        )
        .await
        .expect("profile insert");

        let err = db
            .execute_unprepared(
                "DELETE FROM providers WHERE owner = 'system' AND provider_id = 'p'",
            )
            .await
            .expect_err("the profile still names the provider");
        let mapped = map_db_err(err);
        assert_eq!(mapped.status, 409);
        assert!(
            mapped
                .message
                .contains("cannot be deleted while a profile names it"),
            "the rule names the delete-blocked direction: {}",
            mapped.message
        );
    }

    /// The set arm of the same split, on a real driver error: a profile write
    /// naming a set the collection does not hold reaches the constraint when
    /// the handler's name check loses the race, and the answer names the set
    /// rule rather than a cross-collection reference.
    #[tokio::test]
    async fn a_profile_naming_a_vanished_set_names_the_set_reference() {
        use sea_orm::ConnectionTrait;

        let db = crate::test_support::migrated_test_db().await;
        db.execute_unprepared(
            "INSERT INTO providers (owner, provider_id, family) VALUES ('system', 'p', 'deepseek')",
        )
        .await
        .expect("provider insert");
        let err = db
            .execute_unprepared(
                "INSERT INTO profiles (owner, collection_name, profile_id, provider_id, model, set_name) VALUES ('system', 'baseline', 'p8', 'p', 'm', 'no-such-set')",
            )
            .await
            .expect_err("the named set is not in the collection");
        let mapped = map_db_err(err);
        assert_eq!(mapped.status, 409);
        assert!(
            mapped.message.contains("must exist in its own collection"),
            "the rule names the set reference: {}",
            mapped.message
        );
    }
}
