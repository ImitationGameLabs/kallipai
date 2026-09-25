//! Initial chat_history schema: the `chat_history` table + the
//! `(conversation_id, id)` index the read paths query by.
//!
//! `CREATE ... IF NOT EXISTS` makes the migration idempotent: a single tagma
//! process opens the DB in multiple places, and a re-open after migration
//! must be a no-op (sea-orm-migration records the `seaql_migrations` row on
//! first apply). Subsequent schema changes are new `m_*` files that ALTER the
//! table, never in-place edits here.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS chat_history ( \
                    id INTEGER PRIMARY KEY AUTOINCREMENT, \
                    conversation_id TEXT NOT NULL, \
                    direction TEXT NOT NULL, \
                    kind TEXT NOT NULL, \
                    payload BLOB NOT NULL, \
                    created_at INTEGER NOT NULL \
                 ); \
                 CREATE INDEX IF NOT EXISTS idx_chat_history_conv_id \
                    ON chat_history (conversation_id, id);",
            )
            .await?;
        Ok(())
    }
}
