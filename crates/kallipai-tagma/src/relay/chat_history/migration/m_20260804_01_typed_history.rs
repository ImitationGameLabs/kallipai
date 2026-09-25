//! Convert `chat_history` from an opaque `payload` blob log into a typed event
//! table: explicit `sender_kind` / `sender_id` / `sender_handle` (+ CHECK) and
//! `text` columns replace the serialized `TagmaReply`/`TagmaRequest` blob.
//!
//! Why: with the blob, every wire-type change was a *hidden* schema migration
//! (no migration file, no typed column, legacy rows silently fail to
//! deserialize). Typed columns make schema evolution explicit and keep the
//! agent-free boundary (`sender_kind ∈ {human, agent}`, never a daemon-internal
//! agent id) a
//! physical guarantee. This generalizes the existing `history_id` precedent
//! (the row PK, already a typed column stamped into the wire frame at emit).
//!
//! Strategy: drop the legacy blob table and create the typed one fresh. Legacy
//! rows are NOT preserved: the blob era stored only the serialized wire frame
//! (no typed sender), so backfilling would mean deserializing every row inside
//! a migration — and chat_history is a tagma-local dev/replay artifact, fully
//! re-creatable on reconnect (the app re-pulls from the now-empty store). The
//! project's bold-breaking, pre-release stance accepts this loss; a future
//! phase that needs continuity can add a real backfill migration then.
//!
//! Partial-failure note: sea-orm-migration does not wrap a migration in a
//! transaction on SQLite, and the row is recorded as applied only after `up`
//! returns Ok. If this migration fails partway (e.g. disk full after
//! `CREATE TABLE` but before `CREATE INDEX`), the next boot re-runs it from the
//! top — `DROP TABLE IF EXISTS` discards the half-built table. Any rows written
//! in the failed window are lost; acceptable for a dev/replay store.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Drop the legacy blob table (and its index) and create the typed
        // table fresh. `sender_kind` is CHECK-constrained to the two
        // Participant variants, so the agent-free boundary ("no agent id") is
        // a schema-level guarantee, not a runtime convention.
        db.execute_unprepared(
            "DROP INDEX IF EXISTS idx_chat_history_conv_id; \
             DROP TABLE IF EXISTS chat_history; \
             CREATE TABLE chat_history ( \
                id INTEGER PRIMARY KEY AUTOINCREMENT, \
                conversation_id TEXT NOT NULL, \
                direction TEXT NOT NULL, \
                kind TEXT NOT NULL, \
                sender_kind TEXT NOT NULL CHECK (sender_kind IN ('human','agent')), \
                sender_id TEXT NOT NULL, \
                sender_handle TEXT NOT NULL, \
                text TEXT NOT NULL, \
                created_at INTEGER NOT NULL \
            ); \
             CREATE INDEX IF NOT EXISTS idx_chat_history_conv_id \
                ON chat_history (conversation_id, id);",
        )
        .await?;

        Ok(())
    }
}
