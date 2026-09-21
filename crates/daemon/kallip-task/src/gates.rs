//! The event-boundary hard gates, enforced inside the transaction that
//! performs the gated action.
//!
//! Serialization point: SQLite WAL. Every write transaction takes the
//! write lock with its first statement (see `store::take_write_lock`),
//! so the gate check reads under the lock: two concurrent gated
//! transitions queue on `busy_timeout` instead of racing — the loser
//! waits for the winner to commit, then re-checks the gate against the
//! fresh state.
//!
//! The confirmation gate is cycle-scoped: a cycle starts at the latest
//! `start` or `reopen` transition (whichever is later), and only
//! confirmations filed after it count.

use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder};

use crate::entities::task::{Column as TaskColumn, Entity as TaskEntity};
use crate::entities::task_event::{Column as EventColumn, Entity as EventEntity};

/// Serial gate: an assignee works one task at a time.
/// Returns the blocking task when the assignee already holds an
/// `in_progress` task other than `exclude_id`.
pub async fn serial_gate_blocked(
    tx: &DatabaseTransaction,
    assignee: &str,
    exclude_id: i64,
) -> Result<Option<(i64, String)>, sea_orm::DbErr> {
    let row = TaskEntity::find()
        .filter(TaskColumn::Assignee.eq(assignee))
        .filter(TaskColumn::Status.eq("in_progress"))
        .filter(TaskColumn::Id.ne(exclude_id))
        .one(tx)
        .await?;
    Ok(row.map(|t| (t.id, t.title)))
}

/// Confirmation gate: every registered confirmer must have filed a
/// confirm event (`kind=action`, `name=confirm`) in the current cycle.
/// `confirmers` carries the identity ids fixed at create time; confirm
/// actors are ids by construction, so no normalization pass is needed.
/// Returns the confirmer ids whose current-cycle confirmations are
/// missing; the caller renders them into names for the error face.
pub async fn missing_confirmations(
    tx: &DatabaseTransaction,
    task_id: i64,
    confirmers: &[String],
) -> Result<Vec<String>, sea_orm::DbErr> {
    let boundary = confirmation_boundary(tx, task_id).await?;
    let confirms = EventEntity::find()
        .filter(EventColumn::TaskId.eq(task_id))
        .filter(EventColumn::Kind.eq("action"))
        .filter(EventColumn::Name.eq("confirm"))
        .filter(EventColumn::Id.gt(boundary))
        .all(tx)
        .await?;
    let filed: std::collections::HashSet<String> =
        confirms.iter().filter_map(|e| e.actor.clone()).collect();
    Ok(confirmers
        .iter()
        .filter(|id| !filed.contains(*id))
        .cloned()
        .collect())
}

/// Id of the latest `reopen` transition event for the task; 0 when the
/// task was never reopened.
async fn latest_reopen_event_id(
    tx: &DatabaseTransaction,
    task_id: i64,
) -> Result<i64, sea_orm::DbErr> {
    latest_transition_event_id(tx, task_id, "reopen").await
}

/// Confirmation boundary: the later of the latest `start` and the
/// latest `reopen` transition — the task's most recent entry into
/// `in_progress` through a gated door. `resume` deliberately does not
/// re-base it: a pause/resume pair does not open a new cycle.
async fn confirmation_boundary(
    tx: &DatabaseTransaction,
    task_id: i64,
) -> Result<i64, sea_orm::DbErr> {
    Ok(latest_transition_event_id(tx, task_id, "start")
        .await?
        .max(latest_reopen_event_id(tx, task_id).await?))
}

/// Id of the latest `transition` event with the given name; 0 when none.
async fn latest_transition_event_id(
    tx: &DatabaseTransaction,
    task_id: i64,
    name: &str,
) -> Result<i64, sea_orm::DbErr> {
    let event = EventEntity::find()
        .filter(EventColumn::TaskId.eq(task_id))
        .filter(EventColumn::Kind.eq("transition"))
        .filter(EventColumn::Name.eq(name))
        .order_by_desc(EventColumn::Id)
        .one(tx)
        .await?;
    Ok(event.map(|e| e.id).unwrap_or(0))
}
