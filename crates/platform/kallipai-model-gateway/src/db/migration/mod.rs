//! Programmatic sea-orm migrations for the gateway store.
//!
//! One terminal-state migration (see [`m_20260918_01_init`]) carries the
//! whole schema; new migrations append to `Migrator` from here.
//! Applied at boot via `Migrator::up` (see [`crate::db::connect_and_migrate`]).

pub use sea_orm_migration::prelude::*;

mod m_20260918_01_init;

/// The kallipai-model-gateway migrator. New migrations are appended to `migrations`.
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
    use sea_orm::{ConnectionTrait, Statement};

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

    /// The init's terminal shape: every live table lands, the retired
    /// families (the metering/identity era, the link table, the dropped
    /// evolution leftovers) stay absent, the seeded baseline bundle and
    /// both everyone sentinels exist, the set anchor is NOT NULL, and
    /// the anchor's composite foreign key refuses a cross-space
    /// reference. The single down step drops the whole store.
    #[tokio::test]
    async fn init_creates_the_terminal_shape() {
        let db = raw_test_db().await;
        Migrator::up(&db, None).await.expect("apply");

        let present = |table: &'static str| {
            let db = &db;
            let stmt = Statement::from_string(
                db.get_database_backend(),
                format!("SELECT to_regclass('public.{table}') IS NOT NULL AS present"),
            );
            async move {
                db.query_one(stmt)
                    .await
                    .expect("regclass query")
                    .expect("regclass row")
                    .try_get::<bool>("", "present")
                    .expect("present column")
            }
        };
        for table in [
            "profiles",
            "profile_sets",
            "set_members",
            "providers",
            "provider_credentials",
            "collections",
            "groups",
            "group_members",
            "collection_publications",
            "platform_groups",
            "platform_group_members",
            "platform_publications",
            "gateway_selections",
            "management_events",
        ] {
            assert!(present(table).await, "{table} should exist");
        }
        for gone in [
            "request_audits",
            "proxy_keys",
            "proxy_key_sets",
            "key_lifecycle_events",
            "tagmas",
            "tagma_limits",
            "registry_meta",
            "user_defaults",
            "collection_sets",
        ] {
            assert!(!present(gone).await, "{gone} should not exist");
        }

        // The seeded baseline bundle and both everyone sentinels.
        let count = |sql: &'static str| {
            let db = &db;
            let stmt = Statement::from_string(db.get_database_backend(), sql.to_owned());
            async move {
                db.query_one(stmt)
                    .await
                    .expect("count query")
                    .expect("count row")
                    .try_get::<i64>("", "n")
                    .expect("count column")
            }
        };
        assert_eq!(
            count("SELECT count(*) AS n FROM groups WHERE group_id = 'everyone'").await,
            1,
            "the user-side sentinel is seeded"
        );
        assert_eq!(
            count("SELECT count(*) AS n FROM platform_groups WHERE group_id = 'everyone'").await,
            1,
            "the platform-side sentinel is seeded"
        );
        assert_eq!(
            count(
                "SELECT count(*) AS n FROM collections \
                 WHERE owner = 'system' AND name = 'baseline'"
            )
            .await,
            1,
            "the baseline collection is seeded"
        );
        assert_eq!(
            count(
                "SELECT count(*) AS n FROM platform_publications \
                 WHERE owner = 'system' AND collection_name = 'baseline' \
                 AND group_id = 'everyone'"
            )
            .await,
            1,
            "the baseline publication is seeded"
        );

        // The set anchor is NOT NULL: a set without a collection cannot
        // exist.
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "INSERT INTO collections (owner, name, description) \
             VALUES ('system', 'c0', 'd')"
                .to_owned(),
        ))
        .await
        .expect("seed the collection");
        let loose = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO profile_sets (owner, name, description) \
                 VALUES ('system', 'loose', 'd')"
                    .to_owned(),
            ))
            .await;
        assert!(loose.is_err(), "a set must be born anchored");

        // The anchor's composite foreign key keeps the reference inside
        // the set's own space.
        let cross = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO profile_sets (owner, name, description, collection_name) \
                 VALUES ('admin', 's0', 'd', 'c0')"
                    .to_owned(),
            ))
            .await;
        assert!(cross.is_err(), "a cross-space anchor is refused");

        // The single down step drops the whole store.
        Migrator::down(&db, Some(1)).await.expect("drop the store");
        for table in ["profiles", "profile_sets", "collections", "groups"] {
            assert!(!present(table).await, "{table} is gone after down");
        }
    }

    /// The composite foreign key holds: a profile can only reference a
    /// provider row in its own space.
    #[tokio::test]
    async fn profile_provider_reference_rejects_cross_space() {
        let db = raw_test_db().await;
        Migrator::up(&db, None).await.expect("apply");
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "INSERT INTO providers (owner, provider_id, family) \
             VALUES ('system', 'p9', 'deepseek')"
                .to_owned(),
        ))
        .await
        .expect("seed the provider");
        let result = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO profiles (profile_id, provider_id, model, parked, owner) \
                 VALUES ('x9', 'p9', 'm', false, 'admin')"
                    .to_owned(),
            ))
            .await;
        assert!(result.is_err(), "the cross-space reference is refused");
    }

    /// Set names are globally unique: a name maps to exactly one space,
    /// so a same-named set is refused even in another space. With
    /// distinct names, each space owns its full position sequence (the
    /// position index is per-space), while one space's duplicated
    /// position is refused.
    #[tokio::test]
    async fn set_names_are_global_and_positions_unique_per_space() {
        let db = raw_test_db().await;
        Migrator::up(&db, None).await.expect("apply");
        for (owner, name) in [("system", "c1"), ("admin", "c2")] {
            db.execute(Statement::from_string(
                db.get_database_backend(),
                format!(
                    "INSERT INTO collections (owner, name, description) \
                     VALUES ('{owner}', '{name}', 'd')"
                ),
            ))
            .await
            .expect("seed the collection");
        }
        let dup = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO profile_sets (owner, name, description, collection_name) \
                 VALUES ('system', 'dup', 'd', 'c1'), ('admin', 'dup', 'd', 'c2')"
                    .to_owned(),
            ))
            .await;
        assert!(dup.is_err(), "a set name maps to exactly one space");
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "INSERT INTO profile_sets (owner, name, description, collection_name) \
             VALUES ('system', 'dup', 'd', 'c1'), ('admin', 'other', 'd', 'c2')"
                .to_owned(),
        ))
        .await
        .expect("distinct names land in distinct spaces");
        for (owner, name, pid) in [("system", "dup", "px"), ("admin", "other", "qx")] {
            db.execute(Statement::from_string(
                db.get_database_backend(),
                format!(
                    "INSERT INTO providers (owner, provider_id, family) \
                     VALUES ('{owner}', '{pid}', 'deepseek')"
                ),
            ))
            .await
            .expect("seed the provider");
            db.execute(Statement::from_string(
                db.get_database_backend(),
                format!(
                    "INSERT INTO profiles (profile_id, provider_id, model, parked, owner) \
                     VALUES ('{pid}', '{pid}', 'm', false, '{owner}')"
                ),
            ))
            .await
            .expect("seed the profile");
            db.execute(Statement::from_string(
                db.get_database_backend(),
                format!(
                    "INSERT INTO set_members (owner, set_name, profile_id, position) \
                     VALUES ('{owner}', '{name}', '{pid}', 0)"
                ),
            ))
            .await
            .expect("the slot lands in each space");
        }
        let clash = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO providers (owner, provider_id, family) \
                 VALUES ('system', 'py', 'deepseek')"
                    .to_owned(),
            ))
            .await;
        assert!(clash.is_ok(), "the second system provider lands");
        let clash = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO profiles (profile_id, provider_id, model, parked, owner) \
                 VALUES ('py', 'py', 'm', false, 'system')"
                    .to_owned(),
            ))
            .await;
        assert!(clash.is_ok(), "the second system profile lands");
        let clash = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO set_members (owner, set_name, profile_id, position) \
                 VALUES ('system', 'dup', 'py', 0)"
                    .to_owned(),
            ))
            .await;
        assert!(clash.is_err(), "one row per position inside a space");
    }

    /// Group names are unique per owner (uq_groups_owner_name): a second
    /// same-named group in one space is refused, while another space's
    /// same-named group lands.
    #[tokio::test]
    async fn group_names_are_unique_per_owner() {
        let db = raw_test_db().await;
        Migrator::up(&db, None).await.expect("apply");
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "INSERT INTO groups (group_id, owner, name) \
             VALUES ('g1', 'user-a', 'team')"
                .to_owned(),
        ))
        .await
        .expect("seed the group");
        let dup = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO groups (group_id, owner, name) \
                 VALUES ('g2', 'user-a', 'team')"
                    .to_owned(),
            ))
            .await;
        assert!(dup.is_err(), "one 'team' per owner");
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "INSERT INTO groups (group_id, owner, name) \
             VALUES ('g3', 'user-b', 'team')"
                .to_owned(),
        ))
        .await
        .expect("another space's 'team' lands");
    }
}
