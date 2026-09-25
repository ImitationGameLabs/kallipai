//! Add the archive partition marker to `tasks`.
//!
//! The two `ALTER TABLE ... ADD COLUMN` statements run inside one
//! explicit transaction: the migrator books each migration at-most-once
//! but does not itself wrap SQLite migrations in a transaction, so a
//! crash between the two columns would leave a half-shaped table.
//! Either both columns exist or neither does.
//! The marker mirrors the `waiting` timing-marker shape: a flat-table flag
//! with its timestamp, never a state-machine state — archiving is a
//! storage/query partition change (done.txt precedent), not a transition.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "BEGIN IMMEDIATE; \
                 ALTER TABLE tasks ADD COLUMN archived INTEGER NOT NULL DEFAULT 0; \
                 ALTER TABLE tasks ADD COLUMN archived_at INTEGER; \
                 COMMIT;",
            )
            .await?;
        Ok(())
    }
}
