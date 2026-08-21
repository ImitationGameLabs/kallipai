//! Final-warn prompt: an optional custom closing message.
//!
//! `''` is the stored form of "use the built-in default", mirroring how the
//! wake_prompt column reads; the store boundary normalizes Option<String>
//! <-> '' in both directions.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE work_schedules \
                 ADD COLUMN final_warn_prompt TEXT NOT NULL DEFAULT '';",
            )
            .await?;
        Ok(())
    }
}
