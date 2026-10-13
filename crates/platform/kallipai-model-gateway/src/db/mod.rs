//! The gateway durable store (sea-orm / Postgres): the connection
//! pool, the migration registry, and the store-side plumbing the
//! business entities -- the profile registry and the credential
//! tables -- hang off.

pub mod migration;

use std::time::Duration;

use anyhow::Result;
use log::LevelFilter;
use sea_orm::{ConnectOptions, Database, DatabaseConnection, DbErr};
use sea_orm_migration::MigratorTrait;
use tracing::{error, info};

/// A cloned handle to the durable store. Cheap to clone (one shared pool).
pub type Db = DatabaseConnection;

/// Connect to Postgres and apply all pending gateway migrations.
///
/// `sql_slow` is the statement duration above which a query is logged at
/// WARN; the connect options are built by `connect_options`.
pub async fn connect_and_migrate(url: &str, sql_slow: Duration) -> Result<Db> {
    let db = Database::connect(connect_options(url, sql_slow)).await?;
    migration::Migrator::up(&db, None).await?;
    info!(
        sql_slow_ms = sql_slow.as_millis(),
        "kallipai-model-gateway connected to Postgres; SQL log tiers: every statement at debug, slow statements at warn, a storage failure at error"
    );
    Ok(db)
}

/// The pool options the gateway connects with: sqlx statement logging on,
/// every statement at DEBUG (the default `info` filter keeps those quiet),
/// a WARN above `sql_slow`, and a storage failure at ERROR through
/// [`map_db_err`].
fn connect_options(url: &str, sql_slow: Duration) -> ConnectOptions {
    let mut options = ConnectOptions::new(url.to_owned());
    options
        .sqlx_logging(true)
        .sqlx_logging_level(LevelFilter::Debug)
        .sqlx_slow_statements_logging_settings(LevelFilter::Warn, sql_slow);
    options
}

/// Map a sea-orm [`DbErr`] to an HTTP error. A storage-level unique
/// violation is a client-side conflict -- two writers raced one globally
/// unique name or id -- and every other DB failure stays a transient
/// server-side fault, logged at ERROR here: this is the single mapping
/// point the crate's handlers route a storage failure through, so the
/// tier is stated once rather than per call site. (A `From<DbErr> for
/// ApiError` impl would violate the orphan rule -- both types are
/// foreign -- so handlers map explicitly, the lesche shape.)
pub fn map_db_err(e: DbErr) -> kallipai_common::protocol::ApiError {
    if is_unique_violation(&e) {
        return kallipai_common::protocol::ApiError::conflict(
            pg_unique_detail(&e).unwrap_or("a row with that identity already exists"),
        );
    }
    error!(error = %e, "database failure: mapping to an internal error response");
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
/// back to the generic text when the driver shape hides it -- the
/// downcast is the fallible one, so a database error from another
/// driver shape answers `None` instead of panicking.
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
            .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
            .and_then(|pg| pg.detail()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use sea_orm::{DbErr, RuntimeErr};
    use tracing_subscriber::prelude::*;

    use super::{connect_options, map_db_err};

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
            let options = connect_options("postgres://user:pass@host/db", slow);
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
}
