//! Membership and order move onto the profile row: `profiles.set_name` and
//! `profiles.position` replace the `set_members` table, and `profiles.parked`
//! goes with it (unassigned is `set_name IS NULL`).
//!
//! Why the flag can go: two bookkeepers wrote membership -- a member row and
//! a boolean -- and the pair could disagree, so a row could be a set member
//! and still unserved. One column cannot disagree with itself: a row is in a
//! set or it is not, and the composite foreign key
//! `(owner, collection_name, set_name)` keeps the set inside the row's own
//! collection. Deleting a set nulls its members' `set_name`
//! (`ON DELETE SET NULL (set_name)`), which leaves the row in no set:
//! `set_name IS NULL`, the state a parked row takes in `up` as well.
//!
//! Data rules (`up`), in the order the backfill applies them:
//!
//! - A parked row lands unassigned, even when member rows exist for it:
//!   its `parked` column records "not served", and `set_name IS NULL` is
//!   the state that says it. Such a row reached its set's member read and
//!   was refused at forwarding, so no caller ever served it.
//! - A served row with one member row takes that row's set and position.
//! - A served row with several member rows keeps the smallest
//!   `(position, name)` and loses the rest. Nothing is lossless here: a
//!   dropped membership shortens another set's failover chain, and a
//!   publication that granted visibility through a dropped set no longer
//!   reaches the row. The precheck names every row this hits before the
//!   backfill runs.
//!
//! Failure reporting: a self-check compares the backfill against the same
//! pick it was computed from, and the foreign key is added behind a query
//! that names offending `(owner, collection_name, set_name)` triples. A
//! migration that fails on a constraint violation alone leaves the operator
//! without the row that caused it.
//!
//! `down` rebuilds `set_members` and the parked flag from the single column:
//! membership and order survive, while the rows the collapse dropped and the
//! "parked but a member" state cannot come back.

use sea_orm::Statement;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();

        // Name the rows the collapse will decide about before deciding.
        report_conflicts(conn).await?;

        for sql in UP_COLUMNS {
            conn.execute_unprepared(sql).await?;
        }
        conn.execute_unprepared(UP_BACKFILL).await?;
        if let Some(drift) = backfill_drift(conn).await? {
            return Err(DbErr::Migration(format!(
                "profile set backfill does not match its own pick; conflicting rows: {drift}"
            )));
        }
        for sql in UP_INDEXES {
            conn.execute_unprepared(sql).await?;
        }
        if let Err(error) = conn.execute_unprepared(UP_FOREIGN_KEY).await {
            let offenders = details(conn, DANGLING_SETS_QUERY).await?;
            return Err(DbErr::Migration(format!(
                "profiles.set_name does not name a set in its own collection: {error}; \
                 conflicting rows: {}",
                offenders.join("; ")
            )));
        }
        conn.execute_unprepared("ALTER TABLE profiles DROP COLUMN parked")
            .await?;
        conn.execute_unprepared("DROP TABLE set_members").await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared(DOWN_TABLE).await?;
        conn.execute_unprepared(DOWN_BACKFILL).await?;
        for sql in DOWN_COLUMNS {
            conn.execute_unprepared(sql).await?;
        }
        Ok(())
    }
}

/// The two columns the profile row gains. `position` is meaningless while
/// `set_name` is null and keeps its last value there (the column-level
/// `SET NULL` can only clear the foreign-key column, so a `CHECK` tying the
/// two together would make deleting a set fail).
const UP_COLUMNS: &[&str] = &[
    "ALTER TABLE profiles ADD COLUMN set_name text",
    "ALTER TABLE profiles ADD COLUMN position integer NOT NULL DEFAULT 0",
];

/// One statement: the `DISTINCT ON` pick and the assignment share their key
/// list, so [`DRIFT_QUERY`] re-derives exactly the rows this wrote.
const UP_BACKFILL: &str = "UPDATE profiles p SET set_name = pick.set_name, position = pick.position \
     FROM (SELECT DISTINCT ON (owner, collection_name, profile_id) \
                  owner, collection_name, profile_id, set_name, position \
             FROM set_members \
            ORDER BY owner, collection_name, profile_id, position, set_name) pick \
    WHERE p.owner = pick.owner AND p.collection_name = pick.collection_name \
      AND p.profile_id = pick.profile_id AND NOT p.parked";

const UP_INDEXES: &[&str] = &[
    "CREATE INDEX idx_profiles_set_position \
         ON profiles (owner, collection_name, set_name, position)",
    "CREATE INDEX idx_profiles_unassigned \
         ON profiles (owner, collection_name) WHERE set_name IS NULL",
];

/// `ON DELETE SET NULL (set_name)` (PostgreSQL 15+) clears the membership
/// column only: deleting a set turns its members into unassigned rows, which
/// is what the user face means by removing a set.
const UP_FOREIGN_KEY: &str = "ALTER TABLE profiles ADD CONSTRAINT fk_profiles_set \
     FOREIGN KEY (owner, collection_name, set_name) \
     REFERENCES profile_sets (owner, collection_name, name) \
     ON UPDATE CASCADE ON DELETE SET NULL (set_name)";

const DOWN_TABLE: &str = "CREATE TABLE set_members (\
         owner text NOT NULL DEFAULT 'system', \
         collection_name text NOT NULL, \
         set_name text NOT NULL, \
         profile_id text NOT NULL, \
         position integer NOT NULL, \
         CONSTRAINT set_members_pkey \
             PRIMARY KEY (owner, collection_name, set_name, profile_id), \
         CONSTRAINT fk_set_members_set FOREIGN KEY (owner, collection_name, set_name) \
             REFERENCES profile_sets (owner, collection_name, name) ON DELETE CASCADE, \
         CONSTRAINT fk_set_members_profile FOREIGN KEY (owner, collection_name, profile_id) \
             REFERENCES profiles (owner, collection_name, profile_id) ON DELETE CASCADE\
     )";

const DOWN_BACKFILL: &str = "INSERT INTO set_members \
         (owner, collection_name, set_name, profile_id, position) \
     SELECT owner, collection_name, set_name, profile_id, position \
       FROM profiles WHERE set_name IS NOT NULL";

/// The table is created and backfilled first, so the unique index lands on
/// populated rows; then the flag takes its value from the column about to go,
/// and only then the foreign key and both columns drop, `set_name` and
/// `position` alike. That unique position index is the legacy shape's own
/// invariant exactly; a store that gained duplicate positions has no faithful
/// rollback and `down` says so by failing here.
const DOWN_COLUMNS: &[&str] = &[
    "CREATE UNIQUE INDEX uq_set_members_set_position \
         ON set_members (owner, collection_name, set_name, position)",
    "CREATE INDEX idx_set_members_owner_collection_set_name \
         ON set_members (owner, collection_name, set_name)",
    "ALTER TABLE profiles ADD COLUMN parked boolean NOT NULL DEFAULT false",
    "UPDATE profiles SET parked = (set_name IS NULL)",
    "ALTER TABLE profiles DROP CONSTRAINT fk_profiles_set",
    "ALTER TABLE profiles DROP COLUMN set_name",
    "ALTER TABLE profiles DROP COLUMN position",
];

/// Rows with more than one member row: each line names the sets it is a
/// member of, so the pick the backfill makes is visible before it happens.
const MULTI_MEMBER_QUERY: &str = "SELECT format('%s/%s/%s keeps the smallest of %s', \
         owner, collection_name, profile_id, \
         string_agg(set_name || '@' || position, ', ' ORDER BY position, set_name)) AS detail \
     FROM set_members GROUP BY owner, collection_name, profile_id \
     HAVING count(*) > 1 ORDER BY owner, collection_name, profile_id";

/// The third state: parked and still a member. It lands unassigned: the
/// member read listed it and the parking gate refused it at forwarding.
const THIRD_STATE_QUERY: &str = "SELECT format('%s/%s/%s is parked and a member of %s; it lands unassigned', \
         p.owner, p.collection_name, p.profile_id, string_agg(m.set_name, ', ' ORDER BY m.set_name)) AS detail \
     FROM profiles p \
     JOIN set_members m ON m.owner = p.owner AND m.collection_name = p.collection_name \
                       AND m.profile_id = p.profile_id \
     WHERE p.parked GROUP BY p.owner, p.collection_name, p.profile_id \
     ORDER BY p.owner, p.collection_name, p.profile_id";

const DRIFT_QUERY: &str = "SELECT format('%s/%s/%s: expected set %s, stored %s', \
         p.owner, p.collection_name, p.profile_id, pick.set_name, coalesce(p.set_name, 'none')) AS detail \
     FROM profiles p \
     JOIN (SELECT DISTINCT ON (owner, collection_name, profile_id) \
                  owner, collection_name, profile_id, position, set_name \
             FROM set_members \
            ORDER BY owner, collection_name, profile_id, position, set_name) pick \
       ON p.owner = pick.owner AND p.collection_name = pick.collection_name \
      AND p.profile_id = pick.profile_id \
     WHERE NOT p.parked AND p.set_name IS DISTINCT FROM pick.set_name \
     ORDER BY p.owner, p.collection_name, p.profile_id LIMIT 20";

const DANGLING_SETS_QUERY: &str = "SELECT format('%s/%s/%s -> set %s', \
         p.owner, p.collection_name, p.profile_id, p.set_name) AS detail \
     FROM profiles p \
     WHERE p.set_name IS NOT NULL \
       AND NOT EXISTS (SELECT 1 FROM profile_sets s \
                        WHERE s.owner = p.owner AND s.collection_name = p.collection_name \
                          AND s.name = p.set_name) \
     ORDER BY p.owner, p.collection_name, p.profile_id LIMIT 20";

/// Run one detail query and collect its `detail` column.
async fn details<C: ConnectionTrait>(conn: &C, sql: &str) -> Result<Vec<String>, DbErr> {
    let rows = conn
        .query_all(Statement::from_string(
            conn.get_database_backend(),
            sql.to_owned(),
        ))
        .await?;
    rows.iter().map(|row| row.try_get("", "detail")).collect()
}

/// Log every row the collapse decides about. The lines are the only record of
/// why a row ends up in one set and not another, so they are emitted before
/// the backfill rather than on request.
pub(super) async fn report_conflicts<C: ConnectionTrait>(conn: &C) -> Result<(), DbErr> {
    for detail in details(conn, MULTI_MEMBER_QUERY).await? {
        tracing::warn!("profile set collapse: {detail}");
    }
    for detail in details(conn, THIRD_STATE_QUERY).await? {
        tracing::warn!("profile set collapse: {detail}");
    }
    Ok(())
}

/// The doubt the backfill raises about itself: a row whose stored set differs
/// from the pick it was written from. Reported as a list, not as a count.
async fn backfill_drift<C: ConnectionTrait>(conn: &C) -> Result<Option<String>, DbErr> {
    let drift = details(conn, DRIFT_QUERY).await?;
    Ok((!drift.is_empty()).then(|| drift.join("; ")))
}
