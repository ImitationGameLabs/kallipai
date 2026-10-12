//! The write verbs: create, confirm, review, pause, resume, start, close,
//! reopen and note. Each runs inside one transaction that takes the write
//! lock with its first statement.

use super::tx::{
    BUSY_RETRIES, append_event_tx, apply_transition_tx, check_transition, confirm_payload,
    count_tar_entries, flat_txn, is_busy_conn, is_busy_err, load, note_payload, now, parse_status,
    take_write_lock, validate_association,
};
use super::{ConfirmerResolver, TaskStore};
use crate::Error;
use crate::Task;
use crate::entities::task::{ActiveModel, Entity as TaskEntity};
use crate::gates;
use crate::model::{TaskStatus, Transition};
use crate::usage::{self, UsageSnapshot};
use kallipai_blob_store::BlobStore;
use kallipai_common::protocol::{TaskCloseRequest, TaskConfirmRequest, TaskCreateRequest};
use sea_orm::entity::prelude::*;
use sea_orm::{ActiveValue::Set, TransactionError, TransactionTrait};

impl TaskStore {
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
    /// at [`kallipai_common::protocol::REPORT_MAX_BYTES`] — checked
    /// before the retry loop so an oversized report fails without
    /// taking the lock.
    pub async fn confirm(
        &self,
        id: i64,
        actor: &str,
        op: TaskConfirmRequest,
        blobs: std::sync::Arc<dyn BlobStore>,
    ) -> Result<Task, Error> {
        // Ingest before the transaction: the decoded body lands in the
        // content-addressed store first, and the event inside the
        // transaction carries only the pointer. A crash between the two
        // leaves an unreferenced blob - harmless, deduplicated on retry.
        let file_meta = match &op.file {
            Some(file) => {
                use base64::Engine as _;
                let body = base64::engine::general_purpose::STANDARD
                    .decode(&file.file_b64)
                    .map_err(|_| Error::MalformedBase64 {
                        what: "confirm report",
                    })?;
                if std::str::from_utf8(&body).is_err() {
                    return Err(Error::ReportNotUtf8);
                }
                let max = kallipai_common::protocol::REPORT_MAX_BYTES;
                let size = body.len();
                if size > max {
                    return Err(Error::ReportTooLarge { size, max });
                }
                let blob_id = kallipai_blob_store::archive::ingest(blobs.as_ref(), body).await?;
                Some(serde_json::json!({
                    "file_blob": blob_id.as_str(),
                    "file_name": file.file_name,
                    "size": size,
                }))
            }
            None => None,
        };
        let actor = actor.to_owned();
        for attempt in 0..=BUSY_RETRIES {
            let op = op.clone();
            let actor = actor.clone();
            let file_meta = file_meta.clone();
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
                        let payload = confirm_payload(&op.note, file_meta.as_ref());
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
                        apply_transition_tx(tx, row, actor, Transition::Review, None).await
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
    pub async fn pause(
        &self,
        id: i64,
        actor: &str,
        usage: Option<UsageSnapshot>,
    ) -> Result<Task, Error> {
        let actor = actor.to_owned();
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.clone();
            let usage = usage.clone();
            match self
                .db
                .transaction(|tx| {
                    Box::pin(async move {
                        take_write_lock(tx).await?;
                        let row = load(tx, id).await?;
                        let status = parse_status(&row)?;
                        check_transition(Transition::Pause, status, row.id)?;
                        apply_transition_tx(tx, row, actor, Transition::Pause, usage).await
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
    pub async fn resume(
        &self,
        id: i64,
        actor: &str,
        force: bool,
        usage: Option<UsageSnapshot>,
    ) -> Result<Task, Error> {
        let actor = actor.to_owned();
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.clone();
            let usage = usage.clone();
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
                        apply_transition_tx(tx, row, actor, Transition::Resume, usage).await
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
    pub async fn start(
        &self,
        id: i64,
        actor: &str,
        force: bool,
        usage: Option<UsageSnapshot>,
    ) -> Result<Task, Error> {
        let actor = actor.to_owned();
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.clone();
            let usage = usage.clone();
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
                            usage::transition_payload(usage.as_ref())?,
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
    pub async fn close(
        &self,
        id: i64,
        actor: &str,
        req: TaskCloseRequest,
        blobs: std::sync::Arc<dyn BlobStore>,
        usage: Option<UsageSnapshot>,
    ) -> Result<Task, Error> {
        let actor = actor.to_owned();
        // Ingest outside the write transaction: a large archive would
        // otherwise hold the write lock for the whole blob IO. The
        // caller froze the dossier into tar bytes before the request,
        // so closing never touches the filesystem. A crash between the
        // ingest and the commit leaves an unreferenced blob - harmless,
        // deduplicated by content address on retry; a later reopen +
        // close archives fresh content under a new hash.
        let archive: Option<(String, i64)> = match &req.dossier_tar_b64 {
            Some(tar_b64) => {
                use base64::Engine as _;
                let packed = base64::engine::general_purpose::STANDARD
                    .decode(tar_b64)
                    .map_err(|_| Error::MalformedBase64 {
                        what: "dossier archive",
                    })?;
                let max = kallipai_common::protocol::DOSSIER_MAX_BYTES;
                let size = packed.len();
                if size > max {
                    return Err(Error::DossierTooLarge { size, max });
                }
                let entries = count_tar_entries(&packed)?;
                let blob_id = kallipai_blob_store::archive::ingest(blobs.as_ref(), packed).await?;
                Some((blob_id.as_str().to_string(), entries))
            }
            None => None,
        };
        let archive_hash = archive.as_ref().map(|(hash, _)| hash.clone());
        let archive_entries = archive.as_ref().map(|(_, entries)| *entries);
        let TaskCloseRequest {
            reason,
            summary,
            force,
            ..
        } = req;
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.clone();
            let summary = summary.clone();
            let archive_hash = archive_hash.clone();
            let usage = usage.clone();
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
                            archive_entries: Set(archive_entries),
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
                        usage::insert_snapshot(&mut payload, usage.as_ref())?;
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
    pub async fn reopen(
        &self,
        id: i64,
        actor: &str,
        force: bool,
        usage: Option<UsageSnapshot>,
    ) -> Result<Task, Error> {
        let actor = actor.to_owned();
        for attempt in 0..=BUSY_RETRIES {
            let actor = actor.clone();
            let usage = usage.clone();
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
                            usage::transition_payload(usage.as_ref())?,
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
}
