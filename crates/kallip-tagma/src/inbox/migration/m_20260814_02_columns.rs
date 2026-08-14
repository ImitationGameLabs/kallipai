//! Redesigns `inbox_events` from a duty-gate side effect (truncated summary +
//! DELETE-on-flush) into a first-class message store with full bodies and
//! delivery tracking.
//!
//! Uses a table-rebuild (CREATE new → INSERT SELECT → DROP old → RENAME) in a
//! single multi-statement call so it runs on one connection (avoids pool race).
//! Works on ALL SQLite versions (no version-gated DROP COLUMN).
//!
//! Guarded: if `summary` column is gone, the migration is already applied.
//!
//! Legacy rows: the old `flush()` deleted consumed rows, so any surviving row
//! is by definition UNDELIVERED. We set `delivered = 0` so the new
//! `pull_undelivered` path picks them up.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Idempotency guard: if `summary` column is gone, already migrated.
        if !manager.has_column("inbox_events", "summary").await? {
            return Ok(());
        }

        // Single multi-statement call — same pattern as m_01 init migration.
        // Running all DDL in one call ensures a single connection from the pool.
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE inbox_events_new ( \
                    id INTEGER PRIMARY KEY AUTOINCREMENT, \
                    agent_id TEXT NOT NULL, \
                    timestamp INTEGER NOT NULL, \
                    source TEXT NOT NULL, \
                    body TEXT NOT NULL DEFAULT '', \
                    status TEXT NOT NULL DEFAULT 'unread', \
                    delivered INTEGER NOT NULL DEFAULT 0 \
                ); \
                INSERT INTO inbox_events_new \
                    (id, agent_id, timestamp, source, body, status, delivered) \
                 SELECT id, agent_id, timestamp, source, summary, 'unread', 0 \
                 FROM inbox_events; \
                DROP TABLE inbox_events; \
                ALTER TABLE inbox_events_new RENAME TO inbox_events; \
                CREATE INDEX IF NOT EXISTS idx_inbox_events_agent \
                    ON inbox_events (agent_id); \
                CREATE INDEX IF NOT EXISTS idx_inbox_delivered \
                    ON inbox_events (agent_id, delivered)",
            )
            .await?;

        Ok(())
    }
}
