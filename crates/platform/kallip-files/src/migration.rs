//! Programmatic sea-orm migrations for the kallip-files metadata store.
//!
//! One `MigrationTrait` per file, registered in [`Migrator`]. Naming follows
//! ephemera-ai: `m_YYYYMMDD_NN_slug` (the `NN` disambiguates multiple
//! migrations on the same day). Applied at boot via `Migrator::up` (see
//! [`crate::metadata::connect_and_migrate`]).

pub use sea_orm_migration::prelude::*;

mod m_20260831_01_init;

/// The kallip-files migrator. New migrations are appended to `migrations`.
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m_20260831_01_init::Migration)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::raw_test_db;
    use sea_orm::Statement;

    #[tokio::test]
    async fn migrator_applies_reapplies_and_refreshes() {
        let db = raw_test_db().await;

        Migrator::up(&db, None).await.expect("fresh apply");
        // A second up is a no-op: the migration is already recorded.
        Migrator::up(&db, None).await.expect("re-apply is a no-op");

        // Refresh reverts everything and replays it; afterwards the schema
        // must be intact again and exactly the registered migrations logged.
        Migrator::refresh(&db).await.expect("refresh replays");

        let backend = db.get_database_backend();
        let tables = db
            .query_one(Statement::from_string(
                backend,
                "SELECT count(*) AS n FROM information_schema.tables \
                 WHERE table_schema = 'public' \
                 AND table_name IN ('blob_rows', 'file_records', 'delivery_events')"
                    .to_owned(),
            ))
            .await
            .expect("query tables")
            .expect("aggregate row");
        let tables: i64 = tables.try_get("", "n").expect("count column");
        assert_eq!(tables, 3, "refresh must recreate all three tables");

        let logged = db
            .query_one(Statement::from_string(
                backend,
                "SELECT count(*) AS n FROM seaql_migrations".to_owned(),
            ))
            .await
            .expect("query seaql_migrations")
            .expect("aggregate row");
        let logged: i64 = logged.try_get("", "n").expect("count column");
        assert_eq!(logged as usize, Migrator::migrations().len());
    }
}
