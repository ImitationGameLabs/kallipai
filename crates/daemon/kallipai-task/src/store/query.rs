//! The read verbs: list and count, the report payload migration and its
//! reads, single-task fetch, the event trail and the export faces — plus
//! `archive_task`, the one gated write that moves a task out of the list.

use super::rows::{apply_task_filter, to_export, to_row};
use super::tx::{
    BUSY_RETRIES, append_event_tx, flat_txn, is_busy_conn, is_busy_err, load, now, parse_status,
    take_write_lock,
};
use super::{TaskFilter, TaskStore};
use crate::Error;
use crate::entities::task::{ActiveModel, Column as TaskColumn, Entity as TaskEntity};
use crate::entities::task_event::{Column as EventColumn, Entity as EventEntity};
use crate::entities::{task, task_event};
use crate::model::TaskStatus;
use crate::{Task, TaskEvent};
use kallipai_blob_store::{BlobId, BlobStore};
use kallipai_common::protocol::{TaskExport, TaskListPage};
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveValue::Set, DatabaseTransaction, QueryOrder, TransactionError, TransactionTrait,
};

impl TaskStore {
    /// Archives a task: the query partition marker flips and the task
    /// leaves the default list view. The gate requires `closed` —
    /// archiving an open task would hide active work — and `--force`
    /// escapes with an auditable event.
    pub async fn archive_task(&self, id: i64, actor: &str, force: bool) -> Result<Task, Error> {
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.to_owned();
            match self
                .db
                .transaction(|tx| {
                    Box::pin(async move {
                        take_write_lock(tx).await?;
                        let row = load(tx, id).await?;
                        let status = parse_status(&row)?;
                        let escaped = status != TaskStatus::Closed;
                        if escaped && !force {
                            return Err(Error::ArchiveGate {
                                id: row.id,
                                status: status.as_str().to_string(),
                            });
                        }
                        let payload = if escaped {
                            Some(
                                serde_json::json!({
                                    "gate": "archive_requires_closed",
                                    "status": status.as_str(),
                                })
                                .to_string(),
                            )
                        } else {
                            None
                        };
                        let update = ActiveModel {
                            id: Set(row.id),
                            archived: Set(1),
                            archived_at: Set(Some(now())),
                            updated_at: Set(now()),
                            ..Default::default()
                        };
                        update.update(tx).await?;
                        append_event_tx(
                            tx,
                            row.id,
                            "action",
                            "archive",
                            Some(actor),
                            row.assignee.clone(),
                            None,
                            None,
                            payload,
                            now(),
                        )
                        .await?;
                        load(tx, id).await
                    })
                })
                .await
            {
                Err(TransactionError::Connection(ref e))
                    if attempt < BUSY_RETRIES && is_busy_conn(e) =>
                {
                    continue;
                }
                Err(TransactionError::Transaction(ref e))
                    if attempt < BUSY_RETRIES && is_busy_err(e) =>
                {
                    continue;
                }
                other => return other.map_err(flat_txn),
            }
        }
        unreachable!("busy retries are bounded")
    }
    pub async fn list(&self, filter: TaskFilter) -> Result<Vec<Task>, Error> {
        use kallipai_common::protocol::TaskTimeAxis;
        use sea_orm::QuerySelect;
        let mut query = apply_task_filter(TaskEntity::find(), &filter);
        // One axis drives the window, the order, and the caller's time
        // column: newest-first on the axis itself. A NULL axis value (an
        // active task under the closed axis) sorts last and matches no
        // window -- choosing the axis never filters by itself.
        let axis = match filter.time {
            TaskTimeAxis::Updated => TaskColumn::UpdatedAt,
            TaskTimeAxis::Closed => TaskColumn::EndedAt,
        };
        query = query.order_by_desc(axis);
        // Same-second rows tie on the axis stamp; the id breaks the
        // tie deterministically (later creation sorts first).
        query = query.order_by_desc(TaskColumn::Id);
        if let Some(limit) = filter.limit {
            query = query.limit(limit);
        }
        if let Some(offset) = filter.offset {
            query = query.offset(offset);
        }
        Ok(query.all(&self.db).await?)
    }

    /// Row count matching the filter, ignoring limit/offset: the paging
    /// envelope's total.
    pub async fn count(&self, filter: TaskFilter) -> Result<u64, Error> {
        Ok(apply_task_filter(TaskEntity::find(), &filter)
            .count(&self.db)
            .await?)
    }

    /// One-time startup migration for confirm-report payloads: the body
    /// used to live in the event row (the `file` key held the report
    /// text); it moves to the content-addressed blob store and the event
    /// keeps only the pointer plus display metadata. Idempotent by
    /// shape: events already in the pointer form (a `file_blob` key) are
    /// skipped, so a second boot is a no-op. Pre-migration reports had
    /// no file name on record; `report.txt` stands in for display.
    pub async fn migrate_report_payloads(&self, blobs: &dyn BlobStore) -> Result<usize, Error> {
        let confirms = EventEntity::find()
            .filter(EventColumn::Kind.eq("action"))
            .filter(EventColumn::Name.eq("confirm"))
            .all(&self.db)
            .await?;
        let mut migrated = 0usize;
        for event in confirms {
            let Some(raw) = event.payload.as_deref() else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
                continue;
            };
            // Pointer form: already migrated.
            if value.get("file_blob").is_some() {
                continue;
            }
            let Some(body) = value.get("file").and_then(|f| f.as_str()) else {
                continue;
            };
            let size = body.len();
            let blob_id =
                kallipai_blob_store::archive::ingest(blobs, body.as_bytes().to_vec()).await?;
            let mut next = value.clone();
            next["file"] = serde_json::json!({
                "file_blob": blob_id.as_str(),
                "file_name": "report.txt",
                "size": size,
            });
            let mut active: task_event::ActiveModel = event.into();
            active.payload = Set(Some(next.to_string()));
            active.update(&self.db).await?;
            migrated += 1;
        }
        Ok(migrated)
    }

    /// Reads one confirmation report body back: the blob id must appear
    /// in this task's confirm trail, so one task's reports cannot be
    /// read through another task's id. Returns the decoded UTF-8 text.
    pub async fn report_body(
        &self,
        id: i64,
        blob: &str,
        blobs: &dyn BlobStore,
    ) -> Result<String, Error> {
        let confirms = EventEntity::find()
            .filter(EventColumn::TaskId.eq(id))
            .filter(EventColumn::Kind.eq("action"))
            .filter(EventColumn::Name.eq("confirm"))
            .all(&self.db)
            .await?;
        let carried = confirms.iter().any(|e| {
            e.payload
                .as_deref()
                .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                .and_then(|v| v.get("file").and_then(|f| f.get("file_blob")).cloned())
                .and_then(|b| b.as_str().map(str::to_string))
                .is_some_and(|found| found == blob)
        });
        if !carried {
            return Err(Error::NotFound { id });
        }
        let blob_id = BlobId::parse(blob)?;
        let bytes = blobs.get(&blob_id).await?;
        String::from_utf8(bytes).map_err(|_| Error::CorruptRecord {
            id,
            field: "report body",
        })
    }

    /// Ids among `ids` carrying at least one confirm event whose payload
    /// has a `file` key. Exact per-row parse; the input is page-sized.
    pub async fn ids_with_reports(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashSet<i64>, Error> {
        if ids.is_empty() {
            return Ok(Default::default());
        }
        let confirms = EventEntity::find()
            .filter(EventColumn::TaskId.is_in(ids.to_vec()))
            .filter(EventColumn::Kind.eq("action"))
            .filter(EventColumn::Name.eq("confirm"))
            .all(&self.db)
            .await?;
        Ok(confirms
            .into_iter()
            .filter(|e| {
                e.payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                    .and_then(|v| v.get("file").and_then(|f| f.get("file_blob")).cloned())
                    .is_some_and(|r| r.is_string())
            })
            .map(|e| e.task_id)
            .collect())
    }

    /// One paging envelope: the lightweight rows for the page plus the
    /// filter-matched total. The list face of `GET /tasks` — a poll
    /// never drags event trails or report bodies.
    pub async fn list_page(&self, filter: TaskFilter) -> Result<TaskListPage, Error> {
        let rows = self.list(filter.clone()).await?;
        let total = self.count(filter).await?;
        let page_ids: Vec<i64> = rows.iter().map(|t| t.id).collect();
        let with_reports = self.ids_with_reports(&page_ids).await?;
        let mut page_rows = Vec::with_capacity(rows.len());
        for row in rows {
            let has_reports = with_reports.contains(&row.id);
            page_rows.push(to_row(row, has_reports)?);
        }
        Ok(TaskListPage {
            rows: page_rows,
            total: total as i64,
        })
    }

    /// The task row and its trail are a pair: one read transaction keeps
    /// a concurrent write from interleaving between the two queries (a
    /// torn read).
    pub async fn get(&self, id: i64) -> Result<(Task, Vec<TaskEvent>), Error> {
        self.db
            .transaction(|tx| Box::pin(async move { Self::get_in_tx(tx, id).await }))
            .await
            .map_err(flat_txn)
    }

    /// Shared body of the single-task read face, run inside whatever
    /// read transaction the caller opened (get, export).
    async fn get_in_tx(tx: &DatabaseTransaction, id: i64) -> Result<(Task, Vec<TaskEvent>), Error> {
        let task = TaskEntity::find_by_id(id)
            .one(tx)
            .await?
            .ok_or(Error::NotFound { id })?;
        let events = EventEntity::find()
            .filter(task_event::Column::TaskId.eq(id))
            .order_by_asc(task_event::Column::Id)
            .all(tx)
            .await?;
        Ok((task, events))
    }

    pub async fn events_of(&self, id: i64) -> Result<Vec<TaskEvent>, Error> {
        Ok(EventEntity::find()
            .filter(task_event::Column::TaskId.eq(id))
            .order_by_asc(task_event::Column::Id)
            .all(&self.db)
            .await?)
    }

    /// The machine face: the task plus its trail, stable field names, ISO
    /// 8601 UTC times.
    pub async fn export(&self, id: i64) -> Result<TaskExport, Error> {
        self.db
            .transaction(|tx| {
                Box::pin(async move {
                    let (task, events) = Self::get_in_tx(tx, id).await?;
                    to_export(task, events)
                })
            })
            .await
            .map_err(flat_txn)
    }

    /// The machine face for every task, archived included: one pass over
    /// tasks and one over events, grouped per task — no per-task queries.
    pub async fn export_all(&self) -> Result<Vec<TaskExport>, Error> {
        self.db
            .transaction(|tx| {
                Box::pin(async move {
                    let tasks = TaskEntity::find()
                        .order_by_asc(task::Column::Id)
                        .all(tx)
                        .await?;
                    let events = EventEntity::find()
                        .order_by_asc(task_event::Column::Id)
                        .all(tx)
                        .await?;
                    let mut by_task: std::collections::BTreeMap<i64, Vec<TaskEvent>> =
                        std::collections::BTreeMap::new();
                    for e in events {
                        by_task.entry(e.task_id).or_default().push(e);
                    }
                    let mut exports = Vec::with_capacity(tasks.len());
                    for t in tasks {
                        let trail = by_task.remove(&t.id).unwrap_or_default();
                        exports.push(to_export(t, trail)?);
                    }
                    Ok(exports)
                })
            })
            .await
            .map_err(flat_txn)
    }

    /// Content address of a closed archive, for `task extract`.
    pub fn archive_blob_id(task: &Task) -> Result<Option<BlobId>, Error> {
        task.archive_hash
            .as_deref()
            .map(BlobId::parse)
            .transpose()
            .map_err(Error::from)
    }
}
