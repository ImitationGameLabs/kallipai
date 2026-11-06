//! Give every `chat_history` row a per-conversation position (`seq`), plus the
//! per-partition ledger that mints it (`chat_seq`).
//!
//! `id` identifies a row, but it cannot answer "did I miss one": it is one
//! global AUTOINCREMENT, so a row minted in another partition pushes this
//! partition's ids forward and the successor's id is unknowable in advance.
//! `seq` is the answer: inside one partition it is dense and monotonic, so a
//! reader holding the last position it rendered can tell a contiguous
//! successor from a hole.
//!
//! Backfill order is `id` within the partition, which is insertion order.
//! `chat_seq.last_seq` records each partition's backfilled maximum, i.e. the
//! last position handed out, so the next append mints maximum + 1. The ledger is
//! never trimmed by GC: deriving the next number from `MAX(seq)` would reuse
//! numbers once a retention sweep empties a quiet partition.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // `NOT NULL DEFAULT 0` keeps this an append-only column add; the
        // backfill below overwrites every existing row.
        db.execute_unprepared(
            "ALTER TABLE chat_history ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;",
        )
        .await?;
        // Partition-local rank by id, computed in one pass. A correlated
        // COUNT per row would rescan the partition for every row: quadratic in
        // the partition size, inside the migrator's write transaction, on the
        // boot path of an upgrade.
        db.execute_unprepared(
            "UPDATE chat_history SET seq = ranked.rn FROM (
                SELECT id,
                       ROW_NUMBER() OVER (PARTITION BY COALESCE(user_id, '') ORDER BY id) AS rn
                FROM chat_history
             ) AS ranked WHERE ranked.id = chat_history.id;",
        )
        .await?;
        // Two partial indexes: a composite UNIQUE treats NULLs as distinct, so
        // the direct partition (`user_id IS NULL`) would be unprotected.
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS chat_history_seq_direct \
             ON chat_history(seq) WHERE user_id IS NULL;",
        )
        .await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS chat_history_seq_peer \
             ON chat_history(user_id, seq) WHERE user_id IS NOT NULL;",
        )
        .await?;
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS chat_seq ( \
                peer_key TEXT NOT NULL PRIMARY KEY, \
                last_seq INTEGER NOT NULL
            );",
        )
        .await?;
        db.execute_unprepared(
            "INSERT INTO chat_seq (peer_key, last_seq) \
             SELECT COALESCE(user_id, ''), MAX(seq) FROM chat_history \
             GROUP BY COALESCE(user_id, '');",
        )
        .await?;
        Ok(())
    }
}
