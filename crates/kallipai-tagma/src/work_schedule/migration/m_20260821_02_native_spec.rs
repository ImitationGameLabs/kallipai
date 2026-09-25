//! v2 native spec: drop the cron-string and per-agent columns, add the
//! structured spec column. No legacy translation — there is no installed
//! base; cron-era rows are dead weight and go with the columns.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP INDEX IF EXISTS idx_work_schedules_agent; \
                 ALTER TABLE work_schedules DROP COLUMN agent_id; \
                 ALTER TABLE work_schedules DROP COLUMN start_cron; \
                 ALTER TABLE work_schedules DROP COLUMN end_cron; \
                 ALTER TABLE work_schedules DROP COLUMN timezone; \
                 ALTER TABLE work_schedules ADD COLUMN spec TEXT;",
            )
            .await?;
        Ok(())
    }
}
