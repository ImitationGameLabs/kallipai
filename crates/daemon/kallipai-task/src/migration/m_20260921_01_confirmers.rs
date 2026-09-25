//! Confirmers: the waiting mechanism leaves, the seat roster becomes the
//! fixed confirmer roster, and the receipt vocabulary folds into confirm.
//!
//! Schema: drop the `waiting`/`waiting_since` columns, rename `seats` to
//! `confirmers`, and rename the receipt index to match the new event
//! name. All three are ALTER-level (no table rebuild).
//!
//! Event rewrite (idempotent): `receipt` actions become `confirm`
//! actions, and their payload key `report` is renamed to `file` (the
//! key the confirm face reads), `waiting_set`/`waiting_clear` markers
//! are deleted (the mechanism they describe is gone), and
//! `dispatch`/`gate_report`/`chain_op`/`checkpoint`/`annotate` actions
//! are rewritten to `note` with their payloads kept (the rewrite keeps
//! the audit text readable either way).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("ALTER TABLE tasks DROP COLUMN waiting")
            .await?;
        db.execute_unprepared("ALTER TABLE tasks DROP COLUMN waiting_since")
            .await?;
        db.execute_unprepared("ALTER TABLE tasks RENAME COLUMN seats TO confirmers")
            .await?;
        db.execute_unprepared("DROP INDEX IF EXISTS idx_task_events_receipt")
            .await?;
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_task_events_confirm ON task_events (task_id, name, actor)",
        )
        .await?;
        db.execute_unprepared("UPDATE task_events SET name = 'confirm' WHERE name = 'receipt'")
            .await?;
        db.execute_unprepared(
            "DELETE FROM task_events WHERE name IN ('waiting_set', 'waiting_clear')",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE task_events SET name = 'note'
             WHERE name IN ('dispatch', 'gate_report', 'chain_op',
                'checkpoint', 'annotate')",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE task_events SET payload = json_set(payload, '$.file',
              json_extract(payload, '$.report')) WHERE name = 'confirm'
              AND json_extract(payload, '$.report') IS NOT NULL",
        )
        .await?;
        db.execute_unprepared(
            "UPDATE task_events SET payload = json_remove(payload, '$.report')
              WHERE name = 'confirm' AND json_extract(payload, '$.report') IS NOT NULL",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // The rename is a vocabulary decision, not a data transformation:
        // rolling back would resurrect a mechanism the surface no longer
        // speaks. Not supported.
        Err(DbErr::Custom(
            "m_20260921_01_confirmers cannot be rolled back".into(),
        ))
    }
}
