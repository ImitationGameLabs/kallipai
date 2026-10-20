//! Add the `chat_read_cursor` table: one read watermark per conversation
//! partition, keyed by the same `peer_key` the history uses (`user_id`, or the
//! empty string for the direct/operator partition).
//!
//! Why a separate table and not a column on `chat_history`: the watermark is
//! per-partition state, not per-message state — storing it on rows would make
//! "mark read" a write over N rows and leave no place for a partition that has
//! no rows yet. `read_id` is the highest `chat_history.id` the tagma has
//! counted as read; unread is `COUNT(*)` of outbound rows above it.
//!
//! Monotonicity is enforced by the write statement (`MAX`), not by a
//! constraint, so an out-of-order write cannot move the watermark back.
//!
//! Partial-failure note: the table is created with `IF NOT EXISTS`, so a boot
//! that re-runs after a partial failure converges.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS chat_read_cursor ( \
                peer_key TEXT NOT NULL PRIMARY KEY, \
                read_id INTEGER NOT NULL, \
                updated_at INTEGER NOT NULL \
            );",
        )
        .await?;
        Ok(())
    }
}
