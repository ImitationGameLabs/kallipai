//! Dossier archiving moves to close time: the live-path column leaves,
//! the packed entry count arrives.
//!
//! Schema: drop the `dossier_path` column (the live pointer is gone —
//! the caller freezes the dossier into the archive at close and the
//! content address in `archive_hash` is the only pointer), and add
//! `archive_entries` for the tar entry count recorded at close. Both
//! are ALTER-level (no table rebuild). Closed tasks keep their
//! `archive_hash`; nothing else moves.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // One explicit transaction: SQLite migrations get no implicit
        // one from the migration runner (Postgres only), and a crash
        // between the DROP and the ADD would leave the bookkeeping
        // unrecorded — the replayed DROP would then fail on a column
        // already gone, wedging every later boot. Same shape as
        // m_20260908_02_archive.
        db.execute_unprepared(
            "BEGIN IMMEDIATE; \
             ALTER TABLE tasks DROP COLUMN dossier_path; \
             ALTER TABLE tasks ADD COLUMN archive_entries INTEGER; \
             COMMIT;",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "cannot downgrade past m_20260924_01_dossier".to_string(),
        ))
    }
}
