//! `TaskStore`: every verb the CLI exposes, enforced at this layer.
//!
//! Write verbs run inside a transaction that holds the write lock from
//! its first statement (gate check + row update + event insert land
//! together or not at all; see `take_write_lock`). Events carry the dual
//! actor position: `actor` = who triggered the verb, `assignee` = who
//! executes the work at that moment.

use std::borrow::Cow;
use std::path::Path;

use crate::{Task, TaskEvent};
use kallip_blob_store::{BlobId, BlobStore};
use kallip_common::protocol::{
    AssociationExport, EventExport, TaskConfirmRequest, TaskCreateRequest, TaskExport,
    TaskListPage, TaskRow,
};
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveValue::Set, ConnectOptions, Database, DatabaseBackend, DatabaseConnection,
    DatabaseTransaction, QueryOrder, Statement, TransactionError, TransactionTrait,
};
use sea_orm_migration::MigratorTrait as _;
use time::OffsetDateTime;

use crate::entities::task::{ActiveModel, Column as TaskColumn, Entity as TaskEntity};
use crate::entities::task_event::{
    ActiveModel as EventActive, Column as EventColumn, Entity as EventEntity,
};
use crate::entities::{task, task_event};
use crate::gates;
use crate::model::{ClosedReason, TaskStatus, Transition};
use crate::{Error, archive};

/// The name→identity resolver the create verb uses: a sync lookup over a
/// registry snapshot, injected per create call.
pub type ConfirmerResolver = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub status: Option<TaskStatus>,
    pub assignee: Option<String>,
    /// The archive partition: false (default) lists active tasks only,
    /// true lists archived tasks only.
    pub archived: bool,
    /// The time axis for the since/until window, the sort, and the
    /// caller's time column: `updated` (default) = last activity,
    /// `closed` = completion time. Filtering by the axis is the
    /// window's job; the axis itself excludes nothing.
    pub time: kallip_common::protocol::TaskTimeAxis,
    /// The window on the selected axis (epoch seconds, inclusive on
    /// both ends). Rows with a NULL axis value (e.g. `closed` on an
    /// active task) fall outside any window.
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// Page size and page start (both server-side, approval precedent).
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

/// SQLite-backed task store.
#[derive(Clone)]
pub struct TaskStore {
    db: DatabaseConnection,
}

impl TaskStore {
    pub async fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                Error::Other(format!("create task db dir {}: {e}", parent.display()))
            })?;
        }
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let mut opts = ConnectOptions::new(url);
        // 5s busy_timeout x BUSY_RETRIES(3) retries: a write queueing
        // behind sustained contention waits ~20s at worst before the store
        // surfaces the underlying busy error.
        opts.max_connections(4);
        opts.map_sqlx_sqlite_opts(|o| {
            o.journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
                .busy_timeout(std::time::Duration::from_secs(5))
        });
        let db = Database::connect(opts).await?;
        crate::migration::Migrator::up(&db, None).await?;
        Ok(Self { db })
    }

    /// In-memory constructor for tests and scratch use: one connection,
    /// no on-disk file, migrations applied.
    pub async fn open_in_memory() -> Self {
        let mut opts = ConnectOptions::new("sqlite::memory:".to_owned());
        opts.max_connections(1);
        let db = Database::connect(opts).await.expect("in-memory db");
        crate::migration::Migrator::up(&db, None)
            .await
            .expect("migrations");
        Self { db }
    }

    /// Registers a task in the queue (`queued`): the confirmer roster is
    /// resolved to identity ids now, so the close gate can hold it
    /// accountable later. The row and its `create` event land in one
    /// transaction, so the event trail can always derive the flat table.
    pub async fn create(
        &self,
        req: TaskCreateRequest,
        actor: &str,
        resolve_confirmer: ConfirmerResolver,
    ) -> Result<Task, Error> {
        validate_association(&req)?;
        let now = now();
        // An unresolvable confirmer name is a failed create, not a
        // dangling confirmer: the roster is fixed here, in identity-id space.
        let confirmers: Vec<String> = req
            .require
            .iter()
            .map(|confirmer| {
                resolve_confirmer(confirmer).ok_or_else(|| Error::ConfirmerUnresolved {
                    confirmer: confirmer.clone(),
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let confirmers_json = serde_json::to_string(&confirmers)?;
        for attempt in 0..=BUSY_RETRIES {
            let req = req.clone();
            let confirmers_json = confirmers_json.clone();
            let actor = actor.to_owned();
            match self
                .db
                .transaction(|tx| {
                    Box::pin(async move {
                        take_write_lock(tx).await?;
                        let row = ActiveModel {
                            title: Set(req.title),
                            status: Set(TaskStatus::Queued.as_str().to_string()),
                            creator: Set(Some(actor.clone())),
                            assignee: Set(req.assignee.clone()),
                            confirmers: Set(confirmers_json),
                            created_at: Set(now),
                            updated_at: Set(now),
                            dossier_path: Set(req.dossier_path),
                            inbox_id_start: Set(req.inbox_id_start),
                            inbox_id_end: Set(req.inbox_id_end),
                            room_id: Set(req.room_id),
                            room_seq_start: Set(req.room_seq_start),
                            room_seq_end: Set(req.room_seq_end),
                            ..Default::default()
                        };
                        let inserted = TaskEntity::insert(row).exec(tx).await?;
                        append_event_tx(
                            tx,
                            inserted.last_insert_id,
                            "action",
                            "create",
                            Some(actor),
                            req.assignee,
                            None,
                            None,
                            None,
                            now,
                        )
                        .await?;
                        load(tx, inserted.last_insert_id).await
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

    /// Files a confirmation toward the close gate. Allowed while the
    /// task is `in_progress` or `review`. Outside-roster confirmations
    /// are recorded but do not count toward the gate (the roster is
    /// fixed at create). A report may ride along (`file`), size-capped
    /// at [`kallip_common::protocol::REPORT_MAX_BYTES`] — checked
    /// before the retry loop so an oversized report fails without
    /// taking the lock.
    pub async fn confirm(
        &self,
        id: i64,
        actor: &str,
        op: TaskConfirmRequest,
    ) -> Result<Task, Error> {
        if let Some(file) = &op.file {
            let max = kallip_common::protocol::REPORT_MAX_BYTES;
            let size = file.len();
            if size > max {
                return Err(Error::ReportTooLarge { size, max });
            }
        }
        let actor = actor.to_owned();
        for attempt in 0..=BUSY_RETRIES {
            let op = op.clone();
            let actor = actor.clone();
            match self
                .db
                .transaction(|tx| {
                    Box::pin(async move {
                        take_write_lock(tx).await?;
                        let row = load(tx, id).await?;
                        let status = parse_status(&row)?;
                        match status {
                            TaskStatus::InProgress | TaskStatus::Review => {}
                            _ => {
                                return Err(Error::InvalidTransition {
                                    id: row.id,
                                    from: status.as_str().to_string(),
                                    action: "confirm".to_string(),
                                    expected: "in_progress|review".to_string(),
                                });
                            }
                        }
                        let payload = confirm_payload(&op.note, &op.file);
                        append_event_tx(
                            tx,
                            row.id,
                            "action",
                            "confirm",
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

    /// Moves the machine `in_progress -> review`.
    pub async fn review(&self, id: i64, actor: &str) -> Result<Task, Error> {
        let actor = actor.to_owned();
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.clone();
            match self
                .db
                .transaction(|tx| {
                    Box::pin(async move {
                        take_write_lock(tx).await?;
                        let row = load(tx, id).await?;
                        let status = parse_status(&row)?;
                        check_transition(Transition::Review, status, row.id)?;
                        apply_transition_tx(tx, row, actor, Transition::Review).await
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

    /// Moves the machine `in_progress -> paused`. No gate: pausing is
    /// always allowed.
    pub async fn pause(&self, id: i64, actor: &str) -> Result<Task, Error> {
        let actor = actor.to_owned();
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.clone();
            match self
                .db
                .transaction(|tx| {
                    Box::pin(async move {
                        take_write_lock(tx).await?;
                        let row = load(tx, id).await?;
                        let status = parse_status(&row)?;
                        check_transition(Transition::Pause, status, row.id)?;
                        apply_transition_tx(tx, row, actor, Transition::Pause).await
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

    /// Moves the machine `paused -> in_progress`. Runs the serial gate
    /// like `start` and `reopen`; `--force` escapes with an auditable
    /// event. The confirmation window is NOT re-based: a pause/resume
    /// pair does not open a new cycle.
    pub async fn resume(&self, id: i64, actor: &str, force: bool) -> Result<Task, Error> {
        let actor = actor.to_owned();
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.clone();
            match self
                .db
                .transaction(|tx| {
                    Box::pin(async move {
                        take_write_lock(tx).await?;
                        let row = load(tx, id).await?;
                        let status = parse_status(&row)?;
                        check_transition(Transition::Resume, status, row.id)?;
                        if let Some(assignee) = row.assignee.as_deref()
                            && let Some((blocked_by, title)) =
                                gates::serial_gate_blocked(tx, assignee, row.id).await?
                        {
                            if !force {
                                return Err(Error::SerialGate {
                                    assignee: assignee.to_string(),
                                    blocked_by,
                                    title,
                                });
                            }
                            let payload = serde_json::json!({
                                "gate": "serial",
                                "blocked_by": blocked_by,
                                "blocked_title": title,
                            });
                            append_event_tx(
                                tx,
                                row.id,
                                "action",
                                "force_resume",
                                Some(actor.to_string()),
                                row.assignee.clone(),
                                None,
                                None,
                                Some(payload.to_string()),
                                now(),
                            )
                            .await?;
                        }
                        apply_transition_tx(tx, row, actor, Transition::Resume).await
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

    /// Picks a queued task up (`queued -> in_progress`). The serial gate
    /// holds an assignee to one `in_progress` task; `--force` escapes with
    /// an auditable `force_start` event.
    pub async fn start(&self, id: i64, actor: &str, force: bool) -> Result<Task, Error> {
        let actor = actor.to_owned();
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.clone();
            match self
                .db
                .transaction(|tx| {
                    Box::pin(async move {
                        take_write_lock(tx).await?;
                        let row = load(tx, id).await?;
                        let status = parse_status(&row)?;
                        check_transition(Transition::Start, status, id)?;
                        // Pickup assigns the task to the picker when nobody was
                        // named at create.
                        let assignee = row.assignee.clone().unwrap_or_else(|| actor.to_string());

                        if let Some((blocked_by, title)) =
                            gates::serial_gate_blocked(tx, &assignee, row.id).await?
                        {
                            if !force {
                                return Err(Error::SerialGate {
                                    assignee,
                                    blocked_by,
                                    title,
                                });
                            }
                            let payload = serde_json::json!({
                                "gate": "serial",
                                "blocked_by": blocked_by,
                                "blocked_title": title,
                            });
                            append_event_tx(
                                tx,
                                row.id,
                                "action",
                                "force_start",
                                Some(actor.to_string()),
                                Some(assignee.clone()),
                                None,
                                None,
                                Some(payload.to_string()),
                                now(),
                            )
                            .await?;
                        }

                        let update = ActiveModel {
                            id: Set(row.id),
                            status: Set(Transition::Start.to_str().to_string()),
                            assignee: Set(Some(assignee.clone())),
                            started_at: Set(row.started_at.or(Some(now()))),
                            updated_at: Set(now()),
                            ..Default::default()
                        };
                        update.update(tx).await?;
                        append_event_tx(
                            tx,
                            row.id,
                            "transition",
                            "start",
                            Some(actor.to_string()),
                            Some(assignee),
                            Some(status.as_str().to_string()),
                            Some(Transition::Start.to_str().to_string()),
                            None,
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

    /// Closes a task. The close gate requires a confirmation from every
    /// registered confirmer in the current cycle; `--force`
    /// escapes with an auditable event. The dossier (when registered) is
    /// packed canonically and ingested into the blob store before the
    /// transaction opens; the transaction then writes the pointer next to
    /// the gate check, the state flip, and the close event. Packing before
    /// the transaction trades a dossier-change window (TOCTOU) for a
    /// short write lock: the hash is the content address of what was
    /// actually archived, so the pointer is exact for that snapshot even
    /// if the live directory moves on afterwards. Orphan note: a
    /// successful ingest followed by a failed commit leaves the packed
    /// blob unreferenced — bounded growth; the blob store's gc reclaims
    /// on demand but nothing schedules it yet.
    // Seven parameters: the close payload plus one injected dependency
    // (the blob store); a struct would only relocate them, so the lint
    // is answered with an allow.
    #[allow(clippy::too_many_arguments)]
    pub async fn close(
        &self,
        id: i64,
        actor: &str,
        reason: ClosedReason,
        summary: Option<String>,
        force: bool,
        blobs: Option<std::sync::Arc<dyn BlobStore>>,
    ) -> Result<Task, Error> {
        let actor = actor.to_owned();
        // Pack and ingest outside the write transaction: a large dossier
        // would otherwise hold the write lock for the whole archive IO.
        // TOCTOU: the live directory may change between this pack and
        // the commit below. The hash is the content address of the
        // packed snapshot, so the pointer stays exact for what was
        // archived; a later reopen + close archives fresh content under
        // a new hash.
        let row = self.load(id).await?;
        let archive_hash: Option<String> = match (&row.dossier_path, blobs.as_ref()) {
            (Some(dossier), Some(blobs)) => {
                let dir = Path::new(dossier);
                if !dir.is_dir() {
                    return Err(Error::DossierNotDir {
                        path: dossier.clone(),
                    });
                }
                let packed = archive::pack_dir(dir)?;
                let blob_id = archive::ingest(blobs.as_ref(), packed).await?;
                Some(blob_id.as_str().to_string())
            }
            (Some(dossier), None) => {
                return Err(Error::ArchiveNoBlobStore {
                    id,
                    path: dossier.clone(),
                });
            }
            _ => None,
        };
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.clone();
            let summary = summary.clone();
            let archive_hash = archive_hash.clone();
            match self
                .db
                .transaction(|tx| {
                    Box::pin(async move {
                        take_write_lock(tx).await?;
                        let row = load(tx, id).await?;
                        let status = parse_status(&row)?;
                        check_transition(Transition::Close, status, id)?;
                        let confirmers: Vec<String> = serde_json::from_str(&row.confirmers)
                            .map_err(|_| Error::CorruptRecord {
                                id: row.id,
                                field: "confirmers",
                            })?;
                        if !force {
                            let missing =
                                gates::missing_confirmations(tx, row.id, &confirmers).await?;
                            if !missing.is_empty() {
                                return Err(Error::ConfirmationGate {
                                    missing: missing.join(", "),
                                });
                            }
                        } else {
                            let payload = serde_json::json!({
                                "gates": ["confirmations"],
                                "registered_confirmers": confirmers,
                            });
                            append_event_tx(
                                tx,
                                row.id,
                                "action",
                                "force_close",
                                Some(actor.to_string()),
                                row.assignee.clone(),
                                None,
                                None,
                                Some(payload.to_string()),
                                now(),
                            )
                            .await?;
                        }

                        let update = ActiveModel {
                            id: Set(row.id),
                            status: Set(Transition::Close.to_str().to_string()),
                            ended_at: Set(Some(now())),
                            updated_at: Set(now()),
                            closed_reason: Set(Some(reason.as_str().to_string())),
                            close_summary: Set(summary.clone()),
                            archive_hash: Set(archive_hash.clone()),
                            ..Default::default()
                        };
                        update.update(tx).await?;

                        let mut payload = serde_json::json!({ "reason": reason.as_str() });
                        if let Some(summary) = &summary {
                            payload["summary"] = serde_json::Value::String(summary.clone());
                        }
                        if let Some(hash) = &archive_hash {
                            payload["archive_hash"] = serde_json::Value::String(hash.clone());
                        }
                        append_event_tx(
                            tx,
                            row.id,
                            "transition",
                            "close",
                            Some(actor.to_string()),
                            row.assignee.clone(),
                            Some(status.as_str().to_string()),
                            Some(Transition::Close.to_str().to_string()),
                            Some(payload.to_string()),
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

    /// Reworks a closed task (`closed -> in_progress`). The archive stays
    /// content-addressed (unchanged content keeps its hash); the next close
    /// writes a fresh pointer for changed content. History is in the events.
    /// Reopening runs the serial gate like `start` — an assignee still
    /// works one task at a time (`--force` escapes with an auditable
    /// event) — and the `reopen` event doubles as the confirmation-cycle
    /// marker: prior-cycle confirmations no longer satisfy the close gate.
    pub async fn reopen(&self, id: i64, actor: &str, force: bool) -> Result<Task, Error> {
        let actor = actor.to_owned();
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.clone();
            match self
                .db
                .transaction(|tx| {
                    Box::pin(async move {
                        take_write_lock(tx).await?;
                        let row = load(tx, id).await?;
                        let status = parse_status(&row)?;
                        check_transition(Transition::Reopen, status, id)?;
                        if let Some(assignee) = row.assignee.as_deref()
                            && let Some((blocked_by, title)) =
                                gates::serial_gate_blocked(tx, assignee, row.id).await?
                        {
                            if !force {
                                return Err(Error::SerialGate {
                                    assignee: assignee.to_string(),
                                    blocked_by,
                                    title,
                                });
                            }
                            let payload = serde_json::json!({
                                "gate": "serial",
                                "blocked_by": blocked_by,
                                "blocked_title": title,
                            });
                            append_event_tx(
                                tx,
                                row.id,
                                "action",
                                "force_reopen",
                                Some(actor.to_string()),
                                row.assignee.clone(),
                                None,
                                None,
                                Some(payload.to_string()),
                                now(),
                            )
                            .await?;
                        }

                        let update = ActiveModel {
                            id: Set(row.id),
                            status: Set(Transition::Reopen.to_str().to_string()),
                            ended_at: Set(None),
                            updated_at: Set(now()),
                            closed_reason: Set(None),
                            close_summary: Set(None),
                            ..Default::default()
                        };
                        update.update(tx).await?;
                        append_event_tx(
                            tx,
                            row.id,
                            "transition",
                            "reopen",
                            Some(actor.to_string()),
                            row.assignee.clone(),
                            Some(status.as_str().to_string()),
                            Some(Transition::Reopen.to_str().to_string()),
                            None,
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

    /// Appends a note to the trail. Never moves the machine and never
    /// mutates a field: this is the structured append-only record
    /// surface (the event table has no update or delete path). Allowed
    /// in any state, `closed` included — a note is audit-log
    /// material, not a state change.
    pub async fn note(&self, id: i64, actor: &str, note: String) -> Result<Task, Error> {
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.to_owned();
            let note = note.clone();
            match self
                .db
                .transaction(|tx| {
                    Box::pin(async move {
                        take_write_lock(tx).await?;
                        let row = load(tx, id).await?;
                        append_event_tx(
                            tx,
                            row.id,
                            "action",
                            "note",
                            Some(actor),
                            row.assignee.clone(),
                            None,
                            None,
                            note_payload(&Some(note)),
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
        use kallip_common::protocol::TaskTimeAxis;
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
                    .and_then(|v| v.get("file").cloned())
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

    async fn load(&self, id: i64) -> Result<Task, Error> {
        TaskEntity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or(Error::NotFound { id })
    }
}

/// The filter half of the list query (partition, status, assignee, axis
/// window) without order or paging — shared by `list` and `count`.
fn apply_task_filter(
    mut query: sea_orm::Select<TaskEntity>,
    filter: &TaskFilter,
) -> sea_orm::Select<TaskEntity> {
    // The archive partition: the default view is the active one
    // (done.txt precedent); `archived` flips to archived-only.
    query = query.filter(TaskColumn::Archived.eq(i64::from(filter.archived)));
    if let Some(status) = &filter.status {
        query = query.filter(TaskColumn::Status.eq(status.as_str()));
    }
    if let Some(assignee) = &filter.assignee {
        query = query.filter(TaskColumn::Assignee.eq(assignee.as_str()));
    }
    let axis = match filter.time {
        kallip_common::protocol::TaskTimeAxis::Updated => TaskColumn::UpdatedAt,
        kallip_common::protocol::TaskTimeAxis::Closed => TaskColumn::EndedAt,
    };
    if let Some(since) = filter.since {
        query = query.filter(axis.gte(since));
    }
    if let Some(until) = filter.until {
        query = query.filter(axis.lte(until));
    }
    query
}

/// The association export shared by the full export and the list row.
fn association_of(task: &Task) -> Result<Option<AssociationExport>, Error> {
    Ok(
        if task.inbox_id_start.is_some() || task.inbox_id_end.is_some() || task.room_id.is_some() {
            Some(AssociationExport {
                inbox_id_start: task.inbox_id_start,
                inbox_id_end: task.inbox_id_end,
                room_id: task.room_id.clone(),
                room_seq_start: task.room_seq_start,
                room_seq_end: task.room_seq_end,
            })
        } else {
            None
        },
    )
}

fn to_export(task: Task, events: Vec<TaskEvent>) -> Result<TaskExport, Error> {
    let iso = |secs: Option<i64>| {
        secs.map(crate::model::iso8601_utc)
            .transpose()
            .map_err(Error::from)
    };
    let confirmers: Vec<String> = serde_json::from_str(&task.confirmers)?;
    let association = association_of(&task)?;
    let mut event_exports = Vec::with_capacity(events.len());
    for e in events {
        event_exports.push(EventExport {
            id: e.id,
            kind: e.kind.clone(),
            name: e.name.clone(),
            actor: e.actor.clone(),
            actor_role: None,
            assignee: e.assignee.clone(),
            from_status: e.from_status.clone(),
            to_status: e.to_status.clone(),
            payload: e.payload.as_deref().map(serde_json::from_str).transpose()?,
            created_at: iso(Some(e.created_at))?,
        });
    }
    Ok(TaskExport {
        id: task.id,
        title: task.title.clone(),
        status: task.status.clone(),
        creator: task.creator.clone(),
        assignee: task.assignee.clone(),
        confirmers,
        created_at: iso(Some(task.created_at))?,
        updated_at: iso(Some(task.updated_at))?,
        started_at: iso(task.started_at)?,
        ended_at: iso(task.ended_at)?,
        archived: task.archived != 0,
        archived_at: iso(task.archived_at)?,
        closed_reason: task.closed_reason.clone(),
        close_summary: task.close_summary.clone(),
        association,
        dossier_path: task.dossier_path.clone(),
        archive_hash: task.archive_hash.clone(),
        events: event_exports,
    })
}

/// The list-face projection: the store row into the lightweight wire row.
/// `has_reports` comes from the page-level confirmation scan.
fn to_row(task: Task, has_reports: bool) -> Result<TaskRow, Error> {
    let iso = |secs: Option<i64>| {
        secs.map(crate::model::iso8601_utc)
            .transpose()
            .map_err(Error::from)
    };
    let confirmers: Vec<String> = serde_json::from_str(&task.confirmers)?;
    Ok(TaskRow {
        id: task.id,
        title: task.title.clone(),
        status: task.status.clone(),
        creator: task.creator.clone(),
        assignee: task.assignee.clone(),
        confirmers,
        created_at: iso(Some(task.created_at))?,
        updated_at: iso(Some(task.updated_at))?,
        started_at: iso(task.started_at)?,
        ended_at: iso(task.ended_at)?,
        archived: task.archived != 0,
        archived_at: iso(task.archived_at)?,
        closed_reason: task.closed_reason.clone(),
        close_summary: task.close_summary.clone(),
        association: association_of(&task)?,
        dossier_path: task.dossier_path.clone(),
        archive_hash: task.archive_hash.clone(),
        has_reports,
    })
}

async fn load(tx: &DatabaseTransaction, id: i64) -> Result<task::Model, Error> {
    task::Entity::find_by_id(id)
        .one(tx)
        .await?
        .ok_or(Error::NotFound { id })
}

fn parse_status(row: &task::Model) -> Result<TaskStatus, Error> {
    TaskStatus::parse(&row.status).ok_or_else(|| {
        Error::Other(format!(
            "task {} carries unknown status '{}'",
            row.id, row.status
        ))
    })
}

fn check_transition(t: Transition, from: TaskStatus, id: i64) -> Result<(), Error> {
    if t.legal_from().contains(&from) {
        Ok(())
    } else {
        Err(Error::InvalidTransition {
            id,
            from: from.as_str().to_string(),
            action: t.name().to_string(),
            expected: t.legal_from_str(),
        })
    }
}

fn note_payload(note: &Option<String>) -> Option<String> {
    note.as_ref()
        .map(|n| serde_json::json!({ "note": n }).to_string())
}
/// Confirm payload: `note` and `file` side by side, either omissible;
/// None when the confirmation carries neither.
fn confirm_payload(note: &Option<String>, file: &Option<String>) -> Option<String> {
    if note.is_none() && file.is_none() {
        return None;
    }
    let mut payload = serde_json::Map::new();
    if let Some(note) = note {
        payload.insert("note".to_string(), serde_json::Value::String(note.clone()));
    }
    if let Some(file) = file {
        payload.insert("file".to_string(), serde_json::Value::String(file.clone()));
    }
    Some(serde_json::Value::Object(payload).to_string())
}

/// Shared tail of the payload-free transitions (`review`, `pause`,
/// `resume`): flip the status row and append the transition event.
async fn apply_transition_tx(
    tx: &DatabaseTransaction,
    row: task::Model,
    actor: String,
    t: Transition,
) -> Result<Task, Error> {
    let update = ActiveModel {
        id: Set(row.id),
        status: Set(t.to_str().to_string()),
        updated_at: Set(now()),
        ..Default::default()
    };
    update.update(tx).await?;
    append_event_tx(
        tx,
        row.id,
        "transition",
        t.name(),
        Some(actor),
        row.assignee.clone(),
        Some(
            TaskStatus::parse(&row.status)
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| row.status.clone()),
        ),
        Some(t.to_str().to_string()),
        None,
        now(),
    )
    .await?;
    load(tx, row.id).await
}

#[allow(clippy::too_many_arguments)]
async fn append_event_tx(
    db: &impl sea_orm::ConnectionTrait,
    task_id: i64,
    kind: &str,
    name: &str,
    actor: Option<String>,
    assignee: Option<String>,
    from_status: Option<String>,
    to_status: Option<String>,
    payload: Option<String>,
    created_at: i64,
) -> Result<(), Error> {
    let event = EventActive {
        task_id: Set(task_id),
        kind: Set(kind.to_string()),
        name: Set(name.to_string()),
        actor: Set(actor),
        assignee: Set(assignee),
        from_status: Set(from_status),
        to_status: Set(to_status),
        payload: Set(payload),
        created_at: Set(created_at),
        ..Default::default()
    };
    EventEntity::insert(event).exec(db).await?;
    Ok(())
}

fn flat_txn(e: TransactionError<Error>) -> Error {
    match e {
        TransactionError::Connection(e) => Error::Db(e),
        TransactionError::Transaction(e) => e,
    }
}

/// Takes the SQLite write lock at the start of a just-begun deferred
/// transaction — the `BEGIN IMMEDIATE` semantics sea-orm/sqlx do not
/// expose for SQLite. The first statement is a zero-row UPDATE: it
/// grabs the RESERVED write lock before any snapshot read, so gate
/// checks run under the lock and concurrent writers queue on
/// `busy_timeout` instead of failing on a stale-snapshot upgrade
/// (SQLITE_BUSY_SNAPSHOT), which `busy_timeout` cannot cover.
async fn take_write_lock(tx: &DatabaseTransaction) -> Result<(), Error> {
    tx.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "UPDATE tasks SET updated_at = updated_at WHERE id = -1",
    ))
    .await
    .map_err(Error::from)?;
    Ok(())
}

/// Whole-transaction retries for busy-class write failures. The write
/// lock is taken by the transaction's first statement, so a busy is a
/// lock wait that outlived `busy_timeout`, never a half-applied state:
/// a retry re-runs the gate checks against fresh state. Non-busy
/// errors return immediately.
const BUSY_RETRIES: usize = 3;

/// The single busy classifier: every `DbErr` shape that can carry a SQLite
/// error funnels through one code match. SQLITE_BUSY (5) and SQLITE_LOCKED
/// (6) are transient contention a retry can clear; matching by result code
/// (never message text) keeps gate errors that embed user text from
/// masquerading as busy, and vice versa.
fn is_busy_code(code: Option<Cow<'_, str>>) -> bool {
    // SQLite reports extended result codes (517 = BUSY_SNAPSHOT and
    // friends); the primary code is the low byte, and the low byte is
    // what marks transient contention (5 = BUSY, 6 = LOCKED).
    let primary = code
        .as_deref()
        .and_then(|c| c.parse::<u32>().ok())
        .map(|c| c & 0xFF);
    matches!(primary, Some(5) | Some(6))
}

fn is_busy_db(e: &sea_orm::DbErr) -> bool {
    use sea_orm::RuntimeErr;
    let sqlx_err = match e {
        sea_orm::DbErr::Conn(RuntimeErr::SqlxError(e))
        | sea_orm::DbErr::Exec(RuntimeErr::SqlxError(e))
        | sea_orm::DbErr::Query(RuntimeErr::SqlxError(e)) => Some(e),
        _ => None,
    };
    match sqlx_err {
        Some(sqlx::Error::Database(db)) => is_busy_code(db.code()),
        _ => false,
    }
}

fn is_busy_err(e: &Error) -> bool {
    match e {
        Error::Db(db) => is_busy_db(db),
        _ => false,
    }
}

fn is_busy_conn(e: &sea_orm::DbErr) -> bool {
    is_busy_db(e)
}

/// Association keys are paired windows: each range is either fully
/// absent or fully present with `start <= end`, and the seq range
/// needs the room it belongs to. One-sided or inverted windows would
/// silently narrow the trail the task claims (`show` hides one-sided
/// windows outright).
fn validate_association(req: &TaskCreateRequest) -> Result<(), Error> {
    fn window(start: Option<i64>, end: Option<i64>, what: &str) -> Result<(), Error> {
        match (start, end) {
            (None, None) => Ok(()),
            (Some(s), Some(e)) if s <= e => Ok(()),
            (Some(s), Some(e)) => Err(Error::AssociationInvalid {
                detail: format!("{what} window {s}..{e} is inverted"),
            }),
            _ => Err(Error::AssociationInvalid {
                detail: format!("{what} window needs both bounds"),
            }),
        }
    }
    window(req.inbox_id_start, req.inbox_id_end, "inbox id")?;
    window(req.room_seq_start, req.room_seq_end, "room seq")?;
    if req.room_id.is_none() && req.room_seq_start.is_some() {
        return Err(Error::AssociationInvalid {
            detail: "room seq window needs the room id it belongs to".to_string(),
        });
    }
    Ok(())
}

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod busy_tests {
    use super::is_busy_code;
    use std::borrow::Cow;

    #[test]
    fn busy_codes_are_exactly_five_and_six() {
        // SQLITE_BUSY = 5, SQLITE_LOCKED = 6; the extended codes mask
        // down to the same primary byte (261/517 are BUSY-family,
        // 262/518 are LOCKED-family) and must retry just the same.
        assert!(is_busy_code(Some(Cow::Borrowed("5"))));
        assert!(is_busy_code(Some(Cow::Borrowed("6"))));
        assert!(is_busy_code(Some(Cow::Borrowed("261"))));
        assert!(is_busy_code(Some(Cow::Borrowed("517"))));
        assert!(is_busy_code(Some(Cow::Borrowed("262"))));
        assert!(is_busy_code(Some(Cow::Borrowed("518"))));
        // Anything else (constraint 19, corruption 11, non-numeric,
        // no code at all) never retries.
        assert!(!is_busy_code(Some(Cow::Borrowed("19"))));
        assert!(!is_busy_code(Some(Cow::Borrowed("11"))));
        assert!(!is_busy_code(Some(Cow::Borrowed("x"))));
        assert!(!is_busy_code(None));
    }
}
