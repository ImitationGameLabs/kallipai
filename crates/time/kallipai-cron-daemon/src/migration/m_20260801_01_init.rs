//! Initial `schedules` schema.
//!
//! `CREATE ... IF NOT EXISTS` makes the migration idempotent: the daemon opens
//! the DB from several tasks (scheduler + deliverer + HTTP handlers share the
//! pool) and a re-open after migration must be a no-op (sea-orm-migration
//! records the `seaql_migrations` row on first apply). Subsequent schema
//! changes are new `m_*` files appended to `Migrator`, never in-place edits
//! here. The filename is the recorded migration name (`DeriveMigrationName`),
//! so it is immutable once deployed.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS schedules ( \
                    id TEXT PRIMARY KEY, \
                    name TEXT NOT NULL, \
                    trigger TEXT NOT NULL, \
                    agent_id TEXT NOT NULL, \
                    message TEXT NOT NULL, \
                    tags TEXT NOT NULL DEFAULT '[]', \
                    priority TEXT NOT NULL DEFAULT 'normal', \
                    status TEXT NOT NULL DEFAULT 'active', \
                    created_at INTEGER NOT NULL, \
                    next_fire INTEGER, \
                    last_fire INTEGER, \
                    attempts INTEGER NOT NULL DEFAULT 0, \
                    next_attempt_at INTEGER \
                 ); \
                 CREATE INDEX IF NOT EXISTS idx_schedules_status_next_fire \
                    ON schedules (status, next_fire); \
                 CREATE INDEX IF NOT EXISTS idx_schedules_agent \
                    ON schedules (agent_id);",
            )
            .await?;
        Ok(())
    }
}
