//! Programmatic sea-orm migrations for the gateway store.
//!
//! One `MigrationTrait` per file, registered in [`Migrator`]. Naming
//! follows the repo convention: `m_YYYYMMDD_NN_slug` (the `NN` disambiguates
//! multiple migrations on the same day). Applied at boot via `Migrator::up`
//! (see [`crate::db::connect_and_migrate`]).

pub use sea_orm_migration::prelude::*;

mod m_20260918_01_init;

/// The kallip-model-gateway migrator. New migrations are appended to `migrations`.
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m_20260918_01_init::Migration)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::raw_test_db;
    use sea_orm::Statement;

    /// The minimal durable-store round trip: the migrator runs against a
    /// real Postgres, is idempotent (a second `up` is a no-op), and the
    /// migration ledger matches the registry (one row: this schema).
    #[tokio::test]
    async fn migrator_frame_applies_and_reapplies_clean() {
        let db = raw_test_db().await;

        Migrator::up(&db, None).await.expect("fresh apply");
        Migrator::up(&db, None).await.expect("re-apply is a no-op");

        let logged = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "SELECT count(*) AS n FROM seaql_migrations".to_owned(),
            ))
            .await
            .expect("query seaql_migrations")
            .expect("aggregate row");
        let logged: i64 = logged.try_get("", "n").expect("count column");
        assert_eq!(logged as usize, Migrator::migrations().len());
    }

    /// The lifecycle vocabulary is CHECK-enforced. The entity's
    /// closed set inserts; anything else is refused (raw SQL -- the admin
    /// face goes through the entity, so a raw insert is the only way to
    /// reach the constraint directly).
    #[tokio::test]
    async fn lifecycle_event_vocabulary_is_check_enforced() {
        let db = raw_test_db().await;

        Migrator::up(&db, None).await.expect("apply");
        let insert = |event: &str| {
            let sql = format!(
                "INSERT INTO key_lifecycle_events (key_hash, tagma_id, event, created_at) VALUES (decode('00', 'hex'), 't', '{event}', now())"
            );
            Statement::from_string(db.get_database_backend(), sql)
        };
        db.execute(insert("issued"))
            .await
            .expect("vocabulary word inserts");
        let err = db
            .execute(insert("bogus"))
            .await
            .expect_err("CHECK rejects an unknown event");
        assert!(
            err.to_string().contains("ck_key_lifecycle_events_event"),
            "{err}"
        );
    }
}
