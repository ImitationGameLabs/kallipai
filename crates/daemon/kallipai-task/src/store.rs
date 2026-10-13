//! `TaskStore`: every verb the CLI exposes, enforced at this layer.
//!
//! Write verbs run inside a transaction that holds the write lock from
//! its first statement (gate check + row update + event insert land
//! together or not at all; see `tx::take_write_lock`). Events carry the dual
//! actor position: `actor` = who triggered the verb, `assignee` = who
//! executes the work at that moment.
//!
//! The verbs live in submodules: `lifecycle` (the write verbs), `query`
//! (the read verbs, the export faces and the gated archive), `rows` (row
//! assembly) and `tx` (the transaction helpers they share). The types
//! and the two constructors stay here as the module face.

use crate::Error;
use crate::model::TaskStatus;
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait as _;
use std::path::Path;
use std::time::Duration;

mod lifecycle;
mod query;
mod rows;
mod tx;

/// The name→identity resolver the create verb uses: a sync lookup over a
/// registry snapshot, injected per create call.
pub type ConfirmerResolver = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub status: Option<TaskStatus>,
    pub assignee: Option<String>,
    /// The archive partition: false (default) lists active tasks only,
    /// true lists archived tasks only.
    pub archived: bool,
    /// The time axis for the since/until window, the sort, and the
    /// caller's time column: `updated` (default) = last activity,
    /// `closed` = completion time. Filtering by the axis is the
    /// window's job; the axis itself excludes nothing.
    pub time: kallipai_common::protocol::TaskTimeAxis,
    /// The window on the selected axis (epoch seconds, inclusive on
    /// both ends). Rows with a NULL axis value (e.g. `closed` on an
    /// active task) fall outside any window.
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// Page size and page start (both server-side, approval precedent).
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

/// SQLite-backed task store.
#[derive(Clone)]
pub struct TaskStore {
    db: DatabaseConnection,
}

impl TaskStore {
    pub async fn open(path: &Path, sql_slow: Duration) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                Error::Other(format!("create task db dir {}: {e}", parent.display()))
            })?;
        }
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let mut opts = kallipai_db_helper::connect_options(&url, sql_slow);
        // 5s busy_timeout x BUSY_RETRIES(3) retries: a write queueing
        // behind sustained contention waits ~20s at worst before the store
        // surfaces the underlying busy error.
        opts.max_connections(4);
        opts.map_sqlx_sqlite_opts(|o| {
            o.journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
                .busy_timeout(std::time::Duration::from_secs(5))
        });
        let db = Database::connect(opts).await?;
        crate::migration::Migrator::up(&db, None).await?;
        Ok(Self { db })
    }

    /// In-memory constructor for tests and scratch use: one connection,
    /// no on-disk file, migrations applied.
    pub async fn open_in_memory() -> Self {
        let mut opts = ConnectOptions::new("sqlite::memory:".to_owned());
        opts.max_connections(1);
        let db = Database::connect(opts).await.expect("in-memory db");
        crate::migration::Migrator::up(&db, None)
            .await
            .expect("migrations");
        Self { db }
    }
}

#[cfg(test)]
mod busy_tests {
    use super::tx::is_busy_code;
    use std::borrow::Cow;

    #[test]
    fn busy_codes_are_exactly_five_and_six() {
        // SQLITE_BUSY = 5, SQLITE_LOCKED = 6; the extended codes mask
        // down to the same primary byte (261/517 are BUSY-family,
        // 262/518 are LOCKED-family) and must retry just the same.
        assert!(is_busy_code(Some(Cow::Borrowed("5"))));
        assert!(is_busy_code(Some(Cow::Borrowed("6"))));
        assert!(is_busy_code(Some(Cow::Borrowed("261"))));
        assert!(is_busy_code(Some(Cow::Borrowed("517"))));
        assert!(is_busy_code(Some(Cow::Borrowed("262"))));
        assert!(is_busy_code(Some(Cow::Borrowed("518"))));
        // Anything else (constraint 19, corruption 11, non-numeric,
        // no code at all) never retries.
        assert!(!is_busy_code(Some(Cow::Borrowed("19"))));
        assert!(!is_busy_code(Some(Cow::Borrowed("11"))));
        assert!(!is_busy_code(Some(Cow::Borrowed("x"))));
        assert!(!is_busy_code(None));
    }
}
