//! Initial `inbox_events` schema.
//!
//! `CREATE ... IF NOT EXISTS` makes the migration idempotent (re-open after
//! migration is a no-op). All timestamps are i64 unix seconds (UTC), consistent
//! with the other stores. AUTOINCREMENT ensures monotonically increasing ids
//! so flush can ORDER BY id for FIFO ordering and eviction can reliably drop
//! the oldest rows.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS inbox_events ( \
                    id INTEGER PRIMARY KEY AUTOINCREMENT, \
                    agent_id TEXT NOT NULL, \
                    timestamp INTEGER NOT NULL, \
                    source TEXT NOT NULL, \
                    summary TEXT NOT NULL \
                 ); \
                 CREATE INDEX IF NOT EXISTS idx_inbox_events_agent \
                    ON inbox_events (agent_id);",
            )
            .await?;
        Ok(())
    }
}
