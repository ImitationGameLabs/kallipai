//! Add the nullable `attachment` column to `chat_history`: the JSON-serialized
//! `FileAttachment` an inbound user message carried, or `NULL`. Additive —
//! `ALTER TABLE ... ADD COLUMN` keeps every existing row (they replay as
//! attachment-less), matching the wire contract where the field is
//! `serde(default)` both ways.
//!
//! Why a JSON text column and not a side table: the attachment is 1:1 with its
//! row and is only ever read back with it, so a column keeps the read path a
//! plain row mapping with no join.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE chat_history ADD COLUMN attachment TEXT;")
            .await?;
        Ok(())
    }
}
