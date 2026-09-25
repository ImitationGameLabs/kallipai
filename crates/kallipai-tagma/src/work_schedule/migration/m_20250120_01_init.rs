//! Initial `work_schedules` schema.
//!
//! `CREATE ... IF NOT EXISTS` makes the migration idempotent (re-open after
//! migration is a no-op). All timestamps are i64 unix seconds (UTC), consistent
//! with the cron-daemon and chat-history stores.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS work_schedules ( \
                    id TEXT PRIMARY KEY, \
                    name TEXT NOT NULL, \
                    agent_id TEXT NOT NULL, \
                    start_cron TEXT NOT NULL, \
                    end_cron TEXT NOT NULL, \
                    pre_warn_minutes INTEGER NOT NULL DEFAULT 10, \
                    final_warn_minutes INTEGER NOT NULL DEFAULT 5, \
                    wake_prompt TEXT NOT NULL, \
                    status TEXT NOT NULL DEFAULT 'active', \
                    timezone TEXT, \
                    created_at INTEGER NOT NULL \
                 ); \
                 CREATE INDEX IF NOT EXISTS idx_work_schedules_agent \
                    ON work_schedules (agent_id); \
                 CREATE INDEX IF NOT EXISTS idx_work_schedules_status \
                    ON work_schedules (status);",
            )
            .await?;
        Ok(())
    }
}
