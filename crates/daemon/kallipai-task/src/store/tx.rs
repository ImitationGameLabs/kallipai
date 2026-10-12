//! Transaction helpers shared by the verbs: row load and status parsing,
//! transition gates, payload builders, write-lock acquisition with its busy
//! retries, and the association validator.

use crate::Error;
use crate::Task;
use crate::entities::task;
use crate::entities::task::ActiveModel;
use crate::entities::task_event::{ActiveModel as EventActive, Entity as EventEntity};
use crate::model::{TaskStatus, Transition};
use crate::usage::{self, UsageSnapshot};
use kallipai_common::protocol::TaskCreateRequest;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveValue::Set, DatabaseBackend, DatabaseTransaction, Statement, TransactionError,
};
use std::borrow::Cow;
use time::OffsetDateTime;

pub(super) async fn load(tx: &DatabaseTransaction, id: i64) -> Result<task::Model, Error> {
    task::Entity::find_by_id(id)
        .one(tx)
        .await?
        .ok_or(Error::NotFound { id })
}

pub(super) fn parse_status(row: &task::Model) -> Result<TaskStatus, Error> {
    TaskStatus::parse(&row.status).ok_or_else(|| {
        Error::Other(format!(
            "task {} carries unknown status '{}'",
            row.id, row.status
        ))
    })
}

pub(super) fn check_transition(t: Transition, from: TaskStatus, id: i64) -> Result<(), Error> {
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

pub(super) fn note_payload(note: &Option<String>) -> Option<String> {
    note.as_ref()
        .map(|n| serde_json::json!({ "note": n }).to_string())
}
/// Confirm payload: the note and the stored report pointer side by
/// side, either omissible; None when the confirmation carries neither.
/// The report body lives in the blob store - the event carries the
/// content address plus display metadata, never the body.
pub(super) fn confirm_payload(
    note: &Option<String>,
    file_meta: Option<&serde_json::Value>,
) -> Option<String> {
    if note.is_none() && file_meta.is_none() {
        return None;
    }
    let mut payload = serde_json::Map::new();
    if let Some(note) = note {
        payload.insert("note".to_string(), serde_json::Value::String(note.clone()));
    }
    if let Some(file) = file_meta {
        payload.insert("file".to_string(), file.clone());
    }
    Some(serde_json::Value::Object(payload).to_string())
}

/// Counts the entries of an in-memory tar: the entry count is a property
/// of the archived bytes, computed from exactly what gets ingested.
/// Bytes that do not parse as tar are caller input, not a server fault.
pub(super) fn count_tar_entries(packed: &[u8]) -> Result<i64, Error> {
    let mut archive = tar::Archive::new(packed);
    let mut count = 0i64;
    for entry in archive.entries().map_err(|_| Error::MalformedArchive {
        what: "dossier archive",
    })? {
        let _ = entry.map_err(|_| Error::MalformedArchive {
            what: "dossier archive",
        })?;
        count += 1;
    }
    Ok(count)
}

/// Shared tail of the transitions that only flip the status row and append
/// the event: `review`, `pause`, and `resume`. `pause` and `resume` carry a
/// usage snapshot; `review` passes none.
pub(super) async fn apply_transition_tx(
    tx: &DatabaseTransaction,
    row: task::Model,
    actor: String,
    t: Transition,
    usage: Option<UsageSnapshot>,
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
        usage::transition_payload(usage.as_ref())?,
        now(),
    )
    .await?;
    load(tx, row.id).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn append_event_tx(
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

pub(super) fn flat_txn(e: TransactionError<Error>) -> Error {
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
pub(super) async fn take_write_lock(tx: &DatabaseTransaction) -> Result<(), Error> {
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
pub(super) const BUSY_RETRIES: usize = 3;

/// The single busy classifier: every `DbErr` shape that can carry a SQLite
/// error funnels through one code match. SQLITE_BUSY (5) and SQLITE_LOCKED
/// (6) are transient contention a retry can clear; matching by result code
/// (never message text) keeps gate errors that embed user text from
/// masquerading as busy, and vice versa.
pub(super) fn is_busy_code(code: Option<Cow<'_, str>>) -> bool {
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

pub(super) fn is_busy_err(e: &Error) -> bool {
    match e {
        Error::Db(db) => is_busy_db(db),
        _ => false,
    }
}

pub(super) fn is_busy_conn(e: &sea_orm::DbErr) -> bool {
    is_busy_db(e)
}

/// Association keys are paired windows: each range is either fully
/// absent or fully present with `start <= end`, and the seq range
/// needs the room it belongs to. One-sided or inverted windows would
/// silently narrow the trail the task claims (`show` hides one-sided
/// windows outright).
pub(super) fn validate_association(req: &TaskCreateRequest) -> Result<(), Error> {
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

pub(super) fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}
