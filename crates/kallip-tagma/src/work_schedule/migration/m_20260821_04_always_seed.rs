//! Always-on seed: the "unset schedule" state goes away.
//!
//! The always variant lands as the default mode, so every install must hold
//! exactly one schedule row from now on — GET is always Some. Existing rows
//! are operator smoke-test data (no installed base to preserve); clear them
//! and seed the singleton slot.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DELETE FROM work_schedules; \
                 INSERT INTO work_schedules \
                     (id, spec, pre_warn_minutes, final_warn_minutes, \
                      wake_prompt, status, created_at) \
                 VALUES ('seed', '{\"mode\":\"always\"}', 10, 5, '', 'active', \
                         CAST(strftime('%s','now') AS INTEGER));",
            )
            .await?;
        Ok(())
    }
}
