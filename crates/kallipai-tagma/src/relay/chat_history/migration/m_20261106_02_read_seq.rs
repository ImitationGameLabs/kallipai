//! Convert the read watermark from a row id to a conversation position.
//!
//! `read_id` was a `chat_history.id`, so the store had to clamp every write at
//! the partition's newest id: with one global id sequence, another partition's
//! row can name a position this partition will never reach, and an uncapped
//! write would pin the badge at zero for good. `read_seq` is the same watermark
//! in the numbering the conversation actually has, so the clamp becomes "not
//! past this partition's own last position" and stops describing a
//! cross-partition accident.
//!
//! Conversion: the number of the partition's rows with `id <= read_id`, which
//! is exactly how many positions the reader had reached.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // Rename the old table out of the way rather than dropping it first:
        // renaming a freshly created table into a name that a DROP freed in
        // the same transaction trips SQLite's name-collision check.
        db.execute_unprepared("ALTER TABLE chat_read_cursor RENAME TO chat_read_cursor_pre_seq;")
            .await?;
        db.execute_unprepared(
            "CREATE TABLE chat_read_cursor (
                peer_key TEXT NOT NULL PRIMARY KEY,
                read_seq INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO chat_read_cursor (peer_key, read_seq, updated_at)
             SELECT c.peer_key,
                    COALESCE((SELECT COUNT(*) FROM chat_history h WHERE COALESCE(h.user_id, '') = c.peer_key AND h.id <= c.read_id), 0),
                    c.updated_at
             FROM chat_read_cursor_pre_seq c;",
        )
        .await?;
        db.execute_unprepared("DROP TABLE chat_read_cursor_pre_seq;")
            .await?;
        Ok(())
    }
}
