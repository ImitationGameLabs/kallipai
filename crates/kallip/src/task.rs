//! The `kallip task` family: thin rendering over the task-domain API.
//! Everything rides the tagma HTTP face (`/tasks`); the tagma process is
//! the SOLE writer of tasks.sqlite. Every write verb resolves the acting
//! agent from `KALLIP_ID` (or --actor) and passes it down as the event
//! `actor`.

use std::io::Cursor;

use anyhow::{Result, anyhow};
use kallip_client::TagmaClient;
use kallip_common::protocol::{
    ClosedReason, TaskChainOpRequest, TaskCheckpointRequest, TaskCloseRequest, TaskCreateRequest,
    TaskDispatchRequest, TaskExport, TaskForceRequest, TaskListQuery, TaskNoteRequest, TaskStatus,
};

use crate::args::task::{
    TaskChainOpType, TaskCloseReason, TaskCommand, TaskStartArgs, TaskTimeAxisArg,
    parse_time_anchor,
};
use kallip_common::timefmt;

/// The time column for one list row: pick the axis field, then either
/// pass the ISO stamp through or shrink it to a relative distance. `-`
/// marks an absent axis value (an active task has no completion time).
fn axis_stamp(
    axis: TaskTimeAxisArg,
    relative: bool,
    now: u64,
    updated_at: Option<&str>,
    ended_at: Option<&str>,
) -> String {
    let raw = match axis {
        TaskTimeAxisArg::Updated => updated_at,
        TaskTimeAxisArg::Closed => ended_at,
    };
    match raw {
        Some(iso) if relative => match timefmt::parse_utc(iso) {
            Ok(epoch) => timefmt::format_relative(now, epoch),
            Err(_) => iso.to_string(),
        },
        Some(iso) => iso.to_string(),
        None => "-".to_string(),
    }
}

pub async fn run_task(client: &TagmaClient, cmd: &TaskCommand) -> Result<()> {
    match cmd {
        TaskCommand::Start(args) => {
            let actor = task_actor(args.actor.as_deref())?;
            let task = match args.id {
                Some(id) => {
                    if has_dispatch_meta(args) {
                        return Err(anyhow!(
                            "start <id> picks an existing task up; dispatch \
                             metadata (--title and friends) registers a new one"
                        ));
                    }
                    client
                        .task_start(
                            id,
                            &TaskForceRequest {
                                actor,
                                force: args.force,
                            },
                        )
                        .await?
                }
                None => {
                    let title = args.title.as_deref().ok_or_else(|| {
                        anyhow!("give a task id to pick up, or --title to register a new task")
                    })?;
                    client
                        .task_create(&TaskCreateRequest {
                            title: title.to_string(),
                            creator: args.creator.clone().unwrap_or_else(|| actor.clone()),
                            assignee: args.assignee.clone(),
                            seats: args.seats.clone(),
                            dossier_path: args.dossier.as_ref().map(|p| p.display().to_string()),
                            inbox_id_start: args.inbox_start,
                            inbox_id_end: args.inbox_end,
                            room_id: args.room.clone(),
                            room_seq_start: args.room_seq_start,
                            room_seq_end: args.room_seq_end,
                        })
                        .await?
                }
            };
            print_state_line(&task);
        }
        TaskCommand::Checkpoint(args) => {
            let actor = task_actor(args.actor.as_deref())?;
            let waiting = tri_flag(args.waiting, args.no_waiting)?;
            let task = client
                .task_checkpoint(
                    args.id,
                    &TaskCheckpointRequest {
                        actor,
                        note: args.note.clone(),
                        receipt: args.receipt,
                        review: args.review,
                        waiting,
                    },
                )
                .await?;
            print_state_line(&task);
        }
        TaskCommand::Close(args) => {
            let actor = task_actor(args.actor.as_deref())?;
            let task = client
                .task_close(
                    args.id,
                    &TaskCloseRequest {
                        actor,
                        reason: close_reason(args.reason),
                        summary: args.summary.clone(),
                        force: args.force,
                    },
                )
                .await?;
            print_state_line(&task);
            if let Some(hash) = &task.archive_hash {
                println!("archive: {hash}");
            }
            if let Some(summary) = &task.close_summary {
                println!("summary: {summary}");
            }
        }
        TaskCommand::Reopen(args) => {
            let actor = task_actor(args.actor.as_deref())?;
            let task = client
                .task_reopen(
                    args.id,
                    &TaskForceRequest {
                        actor,
                        force: args.force,
                    },
                )
                .await?;
            print_state_line(&task);
        }

        TaskCommand::Annotate(args) => {
            let actor = task_actor(args.actor.as_deref())?;
            let task = client
                .task_annotate(
                    args.id,
                    &TaskNoteRequest {
                        actor,
                        note: args.note.clone(),
                    },
                )
                .await?;
            print_state_line(&task);
        }
        TaskCommand::Dispatch(args) => {
            let actor = task_actor(args.actor.as_deref())?;
            // Blank seat entries are dropped: a blank seat name would
            // ghost the close gate forever. `--seats ""` therefore
            // registers an explicit empty roster; omitting --seats
            // re-affirms the registered seats.
            let seats = args.seats.clone().map(|list| {
                list.into_iter()
                    .filter(|s| !s.trim().is_empty())
                    .collect::<Vec<String>>()
            });
            let task = client
                .task_dispatch(args.id, &TaskDispatchRequest { actor, seats })
                .await?;
            print_state_line(&task);
        }
        TaskCommand::GateReport(args) => {
            let actor = task_actor(args.actor.as_deref())?;
            let task = client
                .task_gate_report(
                    args.id,
                    &TaskNoteRequest {
                        actor,
                        note: args.note.clone(),
                    },
                )
                .await?;
            print_state_line(&task);
        }
        TaskCommand::ChainOp(args) => {
            let actor = task_actor(args.actor.as_deref())?;
            let task = client
                .task_chain_op(
                    args.id,
                    &TaskChainOpRequest {
                        actor,
                        op: chain_op_name(args.op).to_string(),
                        detail: args.detail.clone(),
                        force: args.force,
                    },
                )
                .await?;
            print_state_line(&task);
        }
        TaskCommand::Archive(args) => {
            let actor = task_actor(args.actor.as_deref())?;
            let task = client
                .task_archive(
                    args.id,
                    &TaskForceRequest {
                        actor,
                        force: args.force,
                    },
                )
                .await?;
            print_state_line(&task);
        }
        TaskCommand::List(args) => {
            let status = args
                .status
                .as_deref()
                .map(|s| {
                    TaskStatus::parse(s).ok_or_else(|| {
                        anyhow!("unknown status '{s}' (queued|in_progress|review|closed)")
                    })
                })
                .transpose()?;
            let now = timefmt::now_epoch();
            let since = args
                .since
                .as_deref()
                .map(|s| parse_time_anchor(s, now))
                .transpose()?;
            let until = args
                .until
                .as_deref()
                .map(|s| parse_time_anchor(s, now))
                .transpose()?;
            let query = TaskListQuery {
                archived: args.archived,
                status,
                assignee: args.assignee.clone(),
                time: args.time.into(),
                since: since.map(|v| v as i64),
                until: until.map(|v| v as i64),
                limit: args.limit.map(u64::from),
                offset: args.offset.map(u64::from),
            };
            // A clock anchor up top: list times are core content, and the
            // header calibrates the stamps below it (inbox summary precedent).
            println!("current datetime: {}", timefmt::format_utc(now));
            let tasks = client.task_list(&query).await?;
            if tasks.is_empty() {
                println!("(no tasks)");
            } else {
                for t in &tasks {
                    let shown = axis_stamp(
                        args.time,
                        args.relative_time,
                        now,
                        t.updated_at.as_deref(),
                        t.ended_at.as_deref(),
                    );
                    println!(
                        "{:>4}  {:<11} {:<16} {:<24} {}",
                        t.id,
                        t.status,
                        t.assignee.as_deref().unwrap_or("-"),
                        shown,
                        t.title
                    );
                }
                println!("(showing {})", tasks.len());
            }
        }
        TaskCommand::Show(args) => {
            let export = client.task_show(args.id).await?;
            print_show(&export);
        }
        TaskCommand::Export(args) => {
            let mut exports = Vec::new();
            if args.all {
                exports = client.task_export_all().await?;
            } else {
                let id = args.id.ok_or_else(|| anyhow!("give a task id, or --all"))?;
                exports.push(client.task_show(id).await?);
            }
            if args.json {
                println!("{}", serde_json::to_string_pretty(&exports)?);
            } else {
                for e in &exports {
                    print_show(e);
                    println!();
                }
            }
        }
        TaskCommand::Extract(args) => {
            let export = client.task_show(args.id).await?;
            if export.archive_hash.is_none() {
                return Err(anyhow!("task {} has no closed archive", args.id));
            }
            let bytes = client.task_fetch_archive(args.id).await?;
            let mut archive = tar::Archive::new(Cursor::new(bytes));
            archive.unpack(&args.to)?;
            println!(
                "extracted task {} archive to {}",
                args.id,
                args.to.display()
            );
        }
    }
    Ok(())
}

fn print_state_line(task: &TaskExport) {
    println!("task {} {} '{}'", task.id, task.status, task.title);
}

fn print_show(e: &TaskExport) {
    println!("task {}: '{}'", e.id, e.title);
    println!(
        "status: {}  assignee: {}  creator: {}",
        e.status,
        e.assignee.as_deref().unwrap_or("-"),
        e.creator.as_deref().unwrap_or("-")
    );
    if !e.seats.is_empty() {
        println!("seats: {}", e.seats.join(", "));
    }
    if e.waiting {
        println!(
            "waiting: yes (since {})",
            e.waiting_since.as_deref().unwrap_or("?")
        );
    }
    if let Some(a) = &e.association {
        let mut parts = Vec::new();
        if let (Some(from), Some(to)) = (a.inbox_id_start, a.inbox_id_end) {
            parts.push(format!("inbox {from}..{to}"));
        }
        if let Some(room) = &a.room_id {
            match (a.room_seq_start, a.room_seq_end) {
                (Some(from), Some(to)) => parts.push(format!("room {room} seq {from}..{to}")),
                _ => parts.push(format!("room {room}")),
            }
        }
        if !parts.is_empty() {
            println!("association: {}", parts.join("; "));
        }
    }
    if let Some(path) = &e.dossier_path {
        println!("dossier: {path} (live)");
    }
    if let Some(hash) = &e.archive_hash {
        println!("archive: {hash} (closed)");
    }
    if let Some(reason) = &e.closed_reason {
        match &e.close_summary {
            Some(summary) => println!("closed: {reason} — {summary}"),
            None => println!("closed: {reason}"),
        }
    }
    println!("events:");
    for ev in &e.events {
        let scope = match (&ev.from_status, &ev.to_status) {
            (Some(from), Some(to)) => format!(" ({from} -> {to})"),
            _ => String::new(),
        };
        println!(
            "  {} {} {}{} by {}",
            ev.created_at.as_deref().unwrap_or("?"),
            ev.kind,
            ev.name,
            scope,
            ev.actor.as_deref().unwrap_or("-")
        );
    }
}

fn task_actor(flag: Option<&str>) -> Result<String> {
    flag.map(str::to_string)
        .or_else(|| std::env::var("KALLIP_ID").ok())
        .ok_or_else(|| anyhow!("KALLIP_ID not set and --actor not given"))
}

fn tri_flag(set: bool, clear: bool) -> Result<Option<bool>> {
    match (set, clear) {
        (true, true) => Err(anyhow!("--waiting and --no-waiting are mutually exclusive")),
        (true, false) => Ok(Some(true)),
        (false, true) => Ok(Some(false)),
        (false, false) => Ok(None),
    }
}

fn close_reason(reason: TaskCloseReason) -> ClosedReason {
    match reason {
        TaskCloseReason::Completed => ClosedReason::Completed,
        TaskCloseReason::NotPlanned => ClosedReason::NotPlanned,
        TaskCloseReason::Duplicate => ClosedReason::Duplicate,
    }
}

fn chain_op_name(op: TaskChainOpType) -> &'static str {
    match op {
        TaskChainOpType::Commit => "commit",
        TaskChainOpType::Amend => "amend",
        TaskChainOpType::Rebase => "rebase",
        TaskChainOpType::Reset => "reset",
    }
}

fn has_dispatch_meta(args: &TaskStartArgs) -> bool {
    args.title.is_some()
        || args.creator.is_some()
        || args.assignee.is_some()
        || !args.seats.is_empty()
        || args.dossier.is_some()
        || args.inbox_start.is_some()
        || args.inbox_end.is_some()
        || args.room.is_some()
        || args.room_seq_start.is_some()
        || args.room_seq_end.is_some()
}

#[cfg(test)]
mod list_render_tests {
    use super::*;

    #[test]
    fn updated_axis_reads_updated_at_by_default() {
        let s = axis_stamp(
            TaskTimeAxisArg::Updated,
            false,
            100,
            Some("ISO-U"),
            Some("ISO-E"),
        );
        assert_eq!(s, "ISO-U");
    }

    #[test]
    fn closed_axis_reads_ended_at() {
        let s = axis_stamp(
            TaskTimeAxisArg::Closed,
            false,
            100,
            Some("ISO-U"),
            Some("ISO-E"),
        );
        assert_eq!(s, "ISO-E");
    }

    #[test]
    fn absent_axis_value_renders_as_dash() {
        assert_eq!(
            axis_stamp(TaskTimeAxisArg::Closed, false, 100, Some("ISO-U"), None),
            "-"
        );
    }

    #[test]
    fn relative_mode_shrinks_parsable_stamps_and_keeps_odd_ones() {
        let now = timefmt::now_epoch();
        let iso = timefmt::format_utc(now - 3 * 60 * 60);
        assert!(
            axis_stamp(TaskTimeAxisArg::Updated, true, now, Some(&iso), None).starts_with("3h ago"),
        );
        assert_eq!(
            axis_stamp(
                TaskTimeAxisArg::Updated,
                true,
                now,
                Some("not-a-stamp"),
                None
            ),
            "not-a-stamp"
        );
    }
}
