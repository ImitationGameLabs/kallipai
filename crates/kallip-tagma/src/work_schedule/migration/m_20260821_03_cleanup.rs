//! Post-v2 cleanup, two months of hindsight in one step:
//!
//! - Step 02 dropped the cron-era columns but left cron-era rows in place;
//!   with `spec` NULL they decode as a default Monday full-day schedule and
//!   haunt the dev instance as a ghost schedule. There is no installed base
//!   to preserve — delete them.
//! - The `name` column (and the whole name field through the stack) went
//!   away with the single-schedule model: one schedule needs no label.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DELETE FROM work_schedules WHERE spec IS NULL; \
                 ALTER TABLE work_schedules DROP COLUMN name;",
            )
            .await?;
        Ok(())
    }
}
