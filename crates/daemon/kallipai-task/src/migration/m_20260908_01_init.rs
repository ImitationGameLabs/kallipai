//! Initial `tasks` + `task_events` schema.
//!
//! `CREATE ... IF NOT EXISTS` makes the migration idempotent. All timestamps
//! are i64 unix seconds (UTC), consistent with the other stores. `tasks` is
//! the flat current-state table; `task_events` is the append-only trail —
//! coarse state on the row, detail in the trail (the K8s warning).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS tasks ( \
                    id INTEGER PRIMARY KEY AUTOINCREMENT, \
                    title TEXT NOT NULL, \
                    status TEXT NOT NULL, \
                    creator TEXT, \
                    assignee TEXT, \
                    seats TEXT NOT NULL DEFAULT '[]', \
                    created_at INTEGER NOT NULL, \
                    updated_at INTEGER NOT NULL, \
                    started_at INTEGER, \
                    ended_at INTEGER, \
                    waiting INTEGER NOT NULL DEFAULT 0, \
                    waiting_since INTEGER, \
                    closed_reason TEXT, \
                    close_summary TEXT, \
                    inbox_id_start INTEGER, \
                    inbox_id_end INTEGER, \
                    room_id TEXT, \
                    room_seq_start INTEGER, \
                    room_seq_end INTEGER, \
                    dossier_path TEXT, \
                    archive_hash TEXT \
                 ); \
                 CREATE INDEX IF NOT EXISTS idx_tasks_assignee_status \
                    ON tasks (assignee, status); \
                 \
                 CREATE TABLE IF NOT EXISTS task_events ( \
                    id INTEGER PRIMARY KEY AUTOINCREMENT, \
                    task_id INTEGER NOT NULL, \
                    kind TEXT NOT NULL, \
                    name TEXT NOT NULL, \
                    actor TEXT, \
                    assignee TEXT, \
                    from_status TEXT, \
                    to_status TEXT, \
                    payload TEXT, \
                    created_at INTEGER NOT NULL \
                 ); \
                 CREATE INDEX IF NOT EXISTS idx_task_events_task \
                    ON task_events (task_id); \
                 CREATE INDEX IF NOT EXISTS idx_task_events_receipt \
                    ON task_events (task_id, name, actor);",
            )
            .await?;
        Ok(())
    }
}
