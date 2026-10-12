//! Row assembly: the shared list filter, the association export and the
//! task-to-row and task-to-export conversions.

use super::TaskFilter;
use crate::Error;
use crate::entities::task::{Column as TaskColumn, Entity as TaskEntity};
use crate::{Task, TaskEvent};
use kallipai_common::protocol::{AssociationExport, EventExport, TaskExport, TaskRow};
use sea_orm::entity::prelude::*;

/// The filter half of the list query (partition, status, assignee, axis
/// window) without order or paging — shared by `list` and `count`.
pub(super) fn apply_task_filter(
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
        kallipai_common::protocol::TaskTimeAxis::Updated => TaskColumn::UpdatedAt,
        kallipai_common::protocol::TaskTimeAxis::Closed => TaskColumn::EndedAt,
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

pub(super) fn to_export(task: Task, events: Vec<TaskEvent>) -> Result<TaskExport, Error> {
    let iso = |secs: Option<i64>| {
        secs.map(crate::model::iso8601_utc)
            .transpose()
            .map_err(Error::from)
    };
    let confirmers: Vec<String> = serde_json::from_str(&task.confirmers)?;
    let association = association_of(&task)?;
    let usage = crate::usage::aggregate(&events)?;
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
        archive_hash: task.archive_hash.clone(),
        archive_entries: task.archive_entries,
        usage,
        events: event_exports,
    })
}

/// The list-face projection: the store row into the lightweight wire row.
/// `has_reports` comes from the page-level confirmation scan.
pub(super) fn to_row(task: Task, has_reports: bool) -> Result<TaskRow, Error> {
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
        archive_hash: task.archive_hash.clone(),
        archive_entries: task.archive_entries,
        has_reports,
    })
}
