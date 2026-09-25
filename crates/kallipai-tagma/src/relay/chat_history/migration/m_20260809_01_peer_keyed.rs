//! Re-key `chat_history` from the tagma's own (circular, constant-per-DB)
//! `conversation_id` to the **peer**: `user_id` holds the conversation
//! partner's `ParticipantId` (relay) or `NULL` (direct = the operator), with
//! `username` beside it. `direction` now doubles as the author variant
//! (`inbound` = user, `outbound` = agent), so the `sender_*` triple and `kind`
//! are gone; the agent's identity is never stored (reconstructed at read).
//!
//! Why: the old key was the tagma's own id — constant per DB, carrying no
//! information, and forcing direct (operator) and relay (owner) messages into
//! one partition where they rendered as two different people. A nullable
//! per-peer key separates them cleanly and is multi-channel ready (one
//! partition per `user_id`). Pre-release with no production data, so the table
//! is recreated rather than altered in place; legacy rows are dropped
//! (chat_history is a tagma-local replay artifact, re-creatable on reconnect).
//!
//! Partial-failure note: sea-orm-migration does not wrap a migration in a
//! transaction on SQLite, and the row is recorded as applied only after `up`
//! returns Ok. If this migration fails partway, the next boot re-runs it from
//! the top — `DROP TABLE IF EXISTS` discards the half-built table.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "DROP INDEX IF EXISTS idx_chat_history_conv_id; \
             DROP TABLE IF EXISTS chat_history; \
             CREATE TABLE chat_history ( \
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                user_id TEXT, \
                username TEXT, \
                direction TEXT NOT NULL, \
                text TEXT NOT NULL, \
                created_at INTEGER NOT NULL \
            ); \
             CREATE INDEX IF NOT EXISTS idx_chat_history_user_id \
                ON chat_history (user_id, id);",
        )
        .await?;
        Ok(())
    }
}
