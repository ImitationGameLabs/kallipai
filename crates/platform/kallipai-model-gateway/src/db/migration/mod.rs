//! Programmatic sea-orm migrations for the gateway store.
//!
//! The schema is built by two migrations: [`m_20260918_01_init`] lays the
//! base tables, [`m_20261028_01_collection_isolation`] rebuilds profiles,
//! sets and members with their collection keys; new migrations append to
//! `Migrator` from here.
//! Applied at boot via `Migrator::up` (see [`crate::db::connect_and_migrate`]).

pub use sea_orm_migration::prelude::*;

mod m_20260918_01_init;
mod m_20261028_01_collection_isolation;

/// The kallipai-model-gateway migrator. New migrations are appended to `migrations`.
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m_20260918_01_init::Migration),
            Box::new(m_20261028_01_collection_isolation::Migration),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::raw_test_db;
    use sea_orm::{ConnectionTrait, Statement};

    /// The minimal durable-store round trip: the migrator runs against a
    /// real Postgres, is idempotent (a second `up` is a no-op), and the
    /// migration ledger matches the registry (one row per migration).
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

    /// The chain's terminal shape: every live table lands, the retired
    /// families (the metering/identity era, the link table, the dropped
    /// evolution leftovers) stay absent, the seeded baseline bundle and
    /// both everyone sentinels exist, the set anchor is NOT NULL, and
    /// the anchor's composite foreign key refuses a cross-space
    /// reference. The chain's down steps drop the whole store.
    #[tokio::test]
    async fn chain_creates_the_terminal_shape() {
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

        // Both down steps together drop the whole store.
        Migrator::down(&db, Some(2)).await.expect("drop the store");
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
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "INSERT INTO collections (owner, name, description) VALUES ('admin', 'c0', 'd')"
                .to_owned(),
        ))
        .await
        .expect("seed the collection");
        let result = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO profiles (owner, collection_name, profile_id, provider_id, model, parked) VALUES ('admin', 'c0', 'x9', 'p9', 'm', false)"
                    .to_owned(),
            ))
            .await;
        assert!(result.is_err(), "the cross-space reference is refused");
    }

    /// Set names and positions are unique inside a collection, not
    /// inside a space: one name lands in two collections of a space
    /// and in another space's collection, while a second copy inside
    /// one collection is refused. A membership row cannot reach a
    /// profile of another collection -- the composite foreign key is
    /// the only enforcement point, so no write path can bypass it.
    #[tokio::test]
    async fn set_names_and_positions_are_unique_per_collection() {
        let db = raw_test_db().await;
        Migrator::up(&db, None).await.expect("apply");
        for (owner, name) in [("system", "c1"), ("system", "c2"), ("admin", "c1")] {
            db.execute(Statement::from_string(
                db.get_database_backend(),
                format!(
                    "INSERT INTO collections (owner, name, description) VALUES ('{owner}', '{name}', 'd')"
                ),
            ))
            .await
            .expect("seed the collection");
        }
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "INSERT INTO profile_sets (owner, collection_name, name, description) VALUES ('system', 'c1', 'dup', 'd'), ('system', 'c2', 'dup', 'd'), ('admin', 'c1', 'dup', 'd')"
                .to_owned(),
        ))
        .await
        .expect("the same name lands in three collections");
        let dup = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO profile_sets (owner, collection_name, name, description) VALUES ('system', 'c1', 'dup', 'd')"
                    .to_owned(),
            ))
            .await;
        assert!(dup.is_err(), "one name per collection");

        for (owner, collection_name, profile_id) in [("system", "c1", "px"), ("system", "c2", "qx")]
        {
            db.execute(Statement::from_string(
                db.get_database_backend(),
                format!("INSERT INTO providers (owner, provider_id, family) VALUES ('{owner}', '{profile_id}', 'deepseek')"),
            ))
            .await
            .expect("seed the provider");
            db.execute(Statement::from_string(
                db.get_database_backend(),
                format!("INSERT INTO profiles (owner, collection_name, profile_id, provider_id, model, parked) VALUES ('{owner}', '{collection_name}', '{profile_id}', '{profile_id}', 'm', false)"),
            ))
            .await
            .expect("seed the profile");
            db.execute(Statement::from_string(
                db.get_database_backend(),
                format!("INSERT INTO set_members (owner, collection_name, set_name, profile_id, position) VALUES ('{owner}', '{collection_name}', 'dup', '{profile_id}', 0)"),
            ))
            .await
            .expect("the slot lands in each collection");
        }

        db.execute(Statement::from_string(
            db.get_database_backend(),
            "INSERT INTO providers (owner, provider_id, family) VALUES ('system', 'py', 'deepseek')"
                .to_owned(),
        ))
        .await
        .expect("a second system provider lands");
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "INSERT INTO profiles (owner, collection_name, profile_id, provider_id, model, parked) VALUES ('system', 'c1', 'py', 'py', 'm', false)"
                .to_owned(),
        ))
        .await
        .expect("a second c1 profile lands");

        let taken = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO set_members (owner, collection_name, set_name, profile_id, position) VALUES ('system', 'c1', 'dup', 'py', 0)"
                    .to_owned(),
            ))
            .await;
        assert!(taken.is_err(), "one row per position inside a collection");

        let cross = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO set_members (owner, collection_name, set_name, profile_id, position) VALUES ('system', 'c1', 'dup', 'qx', 1)"
                    .to_owned(),
            ))
            .await;
        assert!(
            cross.is_err(),
            "a member cannot reach another collection's profile"
        );
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

    async fn count(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
        let row = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                sql.to_owned(),
            ))
            .await
            .expect("count query")
            .expect("count row");
        row.try_get("", "n").expect("count column")
    }

    async fn exec(db: &sea_orm::DatabaseConnection, sql: &str) {
        db.execute(Statement::from_string(
            db.get_database_backend(),
            sql.to_owned(),
        ))
        .await
        .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }

    async fn pk_columns(db: &sea_orm::DatabaseConnection, table: &str) -> String {
        let row = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                format!(
                    "SELECT string_agg(a.attname, ',' ORDER BY k.ord) AS cols FROM pg_constraint c JOIN unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) ON true JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum WHERE c.contype = 'p' AND c.conrelid = '{table}'::regclass"
                ),
            ))
            .await
            .expect("primary key query")
            .expect("primary key row");
        row.try_get("", "cols").expect("primary key columns")
    }

    /// (columns, referenced columns, referenced table, ON DELETE, ON UPDATE)
    async fn foreign_key(
        db: &sea_orm::DatabaseConnection,
        name: &str,
    ) -> (String, String, String, String, String) {
        let row = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                format!(
                    "SELECT (SELECT string_agg(a.attname, ',' ORDER BY k.ord) FROM unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum) AS cols, (SELECT string_agg(a.attname, ',' ORDER BY k.ord) FROM unnest(c.confkey) WITH ORDINALITY AS k(attnum, ord) JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.attnum) AS ref_cols, c.confrelid::regclass::text AS ref_table, c.confdeltype::text AS on_delete, c.confupdtype::text AS on_update FROM pg_constraint c WHERE c.conname = '{name}'"
                ),
            ))
            .await
            .expect("foreign key query")
            .expect("foreign key row");
        (
            row.try_get("", "cols").expect("columns"),
            row.try_get("", "ref_cols").expect("referenced columns"),
            row.try_get("", "ref_table").expect("referenced table"),
            row.try_get("", "on_delete").expect("on delete"),
            row.try_get("", "on_update").expect("on update"),
        )
    }

    /// The rebuilt shape: the global unique indexes are gone, the
    /// collection sits in all three keys, collection_name is NOT
    /// NULL, both membership edges cascade, and both resolve to the
    /// real tables rather than to the *_new names they were built
    /// against.
    #[tokio::test]
    async fn collection_isolation_shape() {
        let db = raw_test_db().await;
        Migrator::up(&db, None).await.expect("apply");

        let global = "SELECT count(*) AS n FROM pg_indexes WHERE indexname IN ('uq_profiles_profile_id_global', 'uq_profile_sets_name_global')";
        assert_eq!(
            count(&db, global).await,
            0,
            "the global unique indexes are gone"
        );
        let members = "SELECT count(*) AS n FROM pg_indexes WHERE indexname IN ('uq_set_members_set_position', 'idx_set_members_owner_collection_set_name')";
        assert_eq!(
            count(&db, members).await,
            2,
            "the membership indexes carry the collection"
        );
        let anchored = "SELECT count(*) AS n FROM information_schema.columns WHERE table_name = 'profiles' AND column_name = 'collection_name' AND is_nullable = 'NO'";
        assert_eq!(
            count(&db, anchored).await,
            1,
            "a profile cannot exist without a collection"
        );
        let leftovers = "SELECT count(*) AS n FROM pg_class WHERE relkind = 'r' AND relname IN ('profiles_new', 'profile_sets_new', 'set_members_new')";
        assert_eq!(
            count(&db, leftovers).await,
            0,
            "no rebuild table survives the swap"
        );

        for (table, columns) in [
            ("profiles", "owner,collection_name,profile_id"),
            ("profile_sets", "owner,collection_name,name"),
            ("set_members", "owner,collection_name,set_name,profile_id"),
        ] {
            let key = pk_columns(&db, table).await;
            assert_eq!(key, columns, "{table} primary key");
            assert!(
                key.starts_with("owner,collection_name"),
                "{table}: the key leads with the columns its foreign keys reference"
            );
        }

        let (cols, ref_cols, ref_table, on_delete, _) =
            foreign_key(&db, "fk_set_members_set").await;
        assert_eq!(cols, "owner,collection_name,set_name");
        assert_eq!(ref_cols, "owner,collection_name,name");
        assert_eq!(ref_table, "profile_sets");
        assert_eq!(on_delete, "c", "a deleted set takes its members with it");

        let (cols, ref_cols, ref_table, on_delete, _) =
            foreign_key(&db, "fk_set_members_profile").await;
        assert_eq!(cols, "owner,collection_name,profile_id");
        assert_eq!(ref_cols, "owner,collection_name,profile_id");
        assert_eq!(ref_table, "profiles");
        assert_eq!(
            on_delete, "c",
            "a deleted profile takes its memberships with it"
        );

        let (cols, ref_cols, ref_table, on_delete, on_update) =
            foreign_key(&db, "fk_profiles_provider").await;
        assert_eq!(cols, "provider_id,owner");
        assert_eq!(ref_cols, "provider_id,owner");
        assert_eq!(ref_table, "providers");
        assert_eq!(on_delete, "r", "a serving provider cannot be deleted");
        assert_eq!(on_update, "c", "a renamed provider id follows");

        let (cols, ref_cols, ref_table, on_delete, _) =
            foreign_key(&db, "profiles_owner_collection_name_fkey").await;
        assert_eq!(cols, "owner,collection_name");
        assert_eq!(ref_cols, "owner,name");
        assert_eq!(ref_table, "collections");
        assert_eq!(
            on_delete, "c",
            "a deleted collection takes its profiles with it"
        );
    }

    /// The pre-migration fixture: two collections, one profile shared
    /// by both sets, one memberless unassigned row, and a space that carries a
    /// profile but never created a collection.
    async fn seed_legacy(db: &sea_orm::DatabaseConnection) {
        exec(db, "INSERT INTO collections (owner, name, description) VALUES ('system', 'alpha', 'd'), ('system', 'beta', 'd')").await;
        exec(db, "INSERT INTO profile_sets (owner, name, description, collection_name) VALUES ('system', 's-alpha', 'd', 'alpha'), ('system', 's-beta', 'd', 'beta')").await;
        exec(db, "INSERT INTO providers (owner, provider_id, family) VALUES ('system', 'p', 'deepseek'), ('user-a', 'p', 'deepseek')").await;
        exec(db, "INSERT INTO profiles (owner, profile_id, provider_id, model, parked) VALUES ('system', 'shared', 'p', 'm', false), ('system', 'unassigned-row', 'p', 'm', true), ('user-a', 'solo', 'p', 'm', true)").await;
        exec(db, "INSERT INTO set_members (owner, set_name, profile_id, position) VALUES ('system', 's-alpha', 'shared', 0), ('system', 's-beta', 'shared', 0)").await;
    }

    /// Old-shape rows move into the new shape: the profile that two
    /// collections' sets share becomes one row per collection, a
    /// memberless unassigned row keeps its flag, and a space that never created
    /// a collection gains one named default.
    #[tokio::test]
    async fn collection_isolation_homes_every_profile() {
        let db = raw_test_db().await;
        Migrator::up(&db, Some(1)).await.expect("init only");
        seed_legacy(&db).await;

        Migrator::up(&db, None).await.expect("isolate");

        assert_eq!(
            count(&db, "SELECT count(*) AS n FROM profiles").await,
            4,
            "the shared profile splits; the unassigned row and the user profile land once"
        );
        let split = "SELECT count(*) AS n FROM profiles WHERE owner = 'system' AND profile_id = 'shared' AND collection_name IN ('alpha', 'beta') AND NOT parked";
        assert_eq!(
            count(&db, split).await,
            2,
            "the shared profile serves both collections"
        );
        let unassigned = "SELECT count(*) AS n FROM profiles WHERE owner = 'system' AND profile_id = 'unassigned-row' AND collection_name = 'alpha' AND parked";
        assert_eq!(
            count(&db, unassigned).await,
            1,
            "the unassigned row lies in the smallest collection and stays unassigned"
        );
        let home = "SELECT count(*) AS n FROM collections WHERE owner = 'user-a' AND name = 'default' AND default_set_name IS NULL";
        assert_eq!(
            count(&db, home).await,
            1,
            "a space with profiles gains a home collection with no default set"
        );
        let solo = "SELECT count(*) AS n FROM profiles WHERE owner = 'user-a' AND profile_id = 'solo' AND collection_name = 'default'";
        assert_eq!(
            count(&db, solo).await,
            1,
            "the memberless user profile lands in the home collection"
        );
        let members = "SELECT count(*) AS n FROM set_members WHERE owner = 'system' AND profile_id = 'shared' AND collection_name IN ('alpha', 'beta')";
        assert_eq!(
            count(&db, members).await,
            2,
            "each membership follows its own collection"
        );
        let dangled = "SELECT count(*) AS n FROM set_members m WHERE NOT EXISTS (SELECT 1 FROM profiles p WHERE p.owner = m.owner AND p.collection_name = m.collection_name AND p.profile_id = m.profile_id)";
        assert_eq!(count(&db, dangled).await, 0, "no membership dangles");
        let parked_members = "SELECT count(*) AS n FROM profiles p WHERE p.parked AND EXISTS (SELECT 1 FROM set_members m WHERE m.owner = p.owner AND m.collection_name = p.collection_name AND m.profile_id = p.profile_id)";
        assert_eq!(
            count(&db, parked_members).await,
            0,
            "a served row is never parked"
        );
    }

    /// The downgrade merges back to the old shape -- one row per
    /// profile id, the memberships intact -- and the isolation then
    /// runs again on the recovered data.
    #[tokio::test]
    async fn collection_isolation_round_trips_through_down() {
        let db = raw_test_db().await;
        Migrator::up(&db, Some(1)).await.expect("init only");
        seed_legacy(&db).await;
        Migrator::up(&db, None).await.expect("isolate");

        Migrator::down(&db, Some(1))
            .await
            .expect("roll the isolation back");

        let global = "SELECT count(*) AS n FROM pg_indexes WHERE indexname = 'uq_profiles_profile_id_global'";
        assert_eq!(count(&db, global).await, 1, "the global index is back");
        let column = "SELECT count(*) AS n FROM information_schema.columns WHERE table_name = 'profiles' AND column_name = 'collection_name'";
        assert_eq!(
            count(&db, column).await,
            0,
            "the old shape has no collection column"
        );
        assert_eq!(
            count(&db, "SELECT count(*) AS n FROM profiles").await,
            3,
            "the split profile is one row again"
        );
        assert_eq!(count(&db, "SELECT count(*) AS n FROM set_members").await, 2);
        let dangled = "SELECT count(*) AS n FROM set_members m WHERE NOT EXISTS (SELECT 1 FROM profiles p WHERE p.owner = m.owner AND p.profile_id = m.profile_id)";
        assert_eq!(
            count(&db, dangled).await,
            0,
            "no membership dangles after the merge"
        );

        Migrator::up(&db, None).await.expect("isolate again");
        assert_eq!(
            count(&db, "SELECT count(*) AS n FROM profiles").await,
            4,
            "the second pass splits the recovered rows again"
        );
    }

    /// Collections make a set name unique per collection, so two spaces can
    /// hold one name at the same time. The downgrade has to place both under
    /// the old store's index on the bare name.
    #[tokio::test]
    async fn down_suffixes_a_set_name_two_spaces_share() {
        let db = raw_test_db().await;
        Migrator::up(&db, Some(1)).await.expect("init only");
        Migrator::up(&db, None).await.expect("isolate");
        exec(
            &db,
            "INSERT INTO collections (owner, name, description) VALUES ('system', 'c1', 'd'), ('user-a', 'c1', 'd')",
        )
        .await;
        exec(
            &db,
            "INSERT INTO profile_sets (owner, name, description, collection_name) VALUES ('system', 'alpha', 'd', 'c1'), ('user-a', 'alpha', 'd', 'c1')",
        )
        .await;

        Migrator::down(&db, Some(1))
            .await
            .expect("roll the isolation back");

        assert_eq!(
            count(
                &db,
                "SELECT count(*) AS n FROM profile_sets WHERE owner = 'system' AND name = 'alpha'"
            )
            .await,
            1,
            "the smallest (owner, collection) keeps the plain name"
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) AS n FROM profile_sets WHERE owner = 'user-a' AND name = 'alpha--c1'"
            )
            .await,
            1,
            "the other space takes the collection suffix"
        );
    }

    /// The old store holds one profile per id across every space, while the
    /// isolated store holds one per collection. A rollback that finds two
    /// spaces using one id says so instead of failing on the index build.
    #[tokio::test]
    async fn down_refuses_two_spaces_sharing_a_profile_id() {
        let db = raw_test_db().await;
        Migrator::up(&db, Some(1)).await.expect("init only");
        Migrator::up(&db, None).await.expect("isolate");
        exec(
            &db,
            "INSERT INTO collections (owner, name, description) VALUES ('system', 'c1', 'd'), ('user-a', 'c1', 'd')",
        )
        .await;
        exec(
            &db,
            "INSERT INTO providers (owner, provider_id, family) VALUES ('system', 'p', 'deepseek'), ('user-a', 'p', 'deepseek')",
        )
        .await;
        exec(
            &db,
            "INSERT INTO profiles (owner, collection_name, profile_id, provider_id, model) VALUES ('system', 'c1', 'p1', 'p', 'm'), ('user-a', 'c1', 'p1', 'p', 'm')",
        )
        .await;

        let err = Migrator::down(&db, Some(1))
            .await
            .expect_err("the old shape cannot hold the id twice");

        let text = err.to_string();
        for part in ["p1", "system", "user-a"] {
            assert!(text.contains(part), "{part} missing from: {text}");
        }
        assert_eq!(
            count(&db, "SELECT count(*) AS n FROM profiles").await,
            2,
            "the refused rollback leaves the isolated store untouched"
        );
    }

    /// One profile id, two collections, one copy parked and one served: the
    /// merge keeps the row served (`bool_and` over the parked flags), the
    /// same invariant `up` keeps -- a rollback must not turn a served row
    /// unassigned, and a row every copy parked must stay unassigned.
    #[tokio::test]
    async fn down_keeps_a_row_served_when_one_copy_was() {
        let db = raw_test_db().await;
        Migrator::up(&db, Some(1)).await.expect("init only");
        Migrator::up(&db, None).await.expect("isolate");
        exec(
            &db,
            "INSERT INTO collections (owner, name, description) VALUES ('system', 'c1', 'd'), ('system', 'c2', 'd')",
        )
        .await;
        exec(
            &db,
            "INSERT INTO providers (owner, provider_id, family) VALUES ('system', 'p', 'deepseek')",
        )
        .await;
        exec(
            &db,
            "INSERT INTO profiles (owner, collection_name, profile_id, provider_id, model, parked) VALUES ('system', 'c1', 'p1', 'p', 'm', true), ('system', 'c2', 'p1', 'p', 'm', false), ('system', 'c1', 'p2', 'p', 'm', true), ('system', 'c2', 'p2', 'p', 'm', true)",
        )
        .await;

        Migrator::down(&db, Some(1))
            .await
            .expect("roll the isolation back");

        assert_eq!(
            count(&db, "SELECT count(*) AS n FROM profiles").await,
            2,
            "each id is one row again"
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) AS n FROM profiles WHERE profile_id = 'p1' AND parked = false"
            )
            .await,
            1,
            "one served copy keeps the merged row served"
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) AS n FROM profiles WHERE profile_id = 'p2' AND parked = true"
            )
            .await,
            1,
            "a row every copy parked comes back unassigned"
        );
    }
}
