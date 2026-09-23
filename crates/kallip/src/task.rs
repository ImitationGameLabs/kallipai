//! The `kallip task` family: thin rendering over the task-domain API.
//! Everything rides the tagma HTTP face (`/tasks`); the tagma process is
//! the SOLE writer of tasks.sqlite. Write verbs carry no actor: the
//! tagma records the acting agent from the authenticated identity.

use std::io::Cursor;

use anyhow::{Result, anyhow};
use kallip_client::TagmaClient;
use kallip_common::protocol::{
    ClosedReason, REPORT_MAX_BYTES, TaskCloseRequest, TaskConfirmRequest, TaskCreateRequest,
    TaskExport, TaskForceRequest, TaskListQuery, TaskNoteRequest, TaskStatus,
};

use crate::args::task::{
    TaskCloseReason, TaskCommand, TaskReportCommand, TaskTimeAxisArg, parse_time_anchor,
};
use kallip_common::timefmt;
use kallip_common::timefmt::DisplayZone;

/// Resolve the zone absolute stamps render in: `--utc` wins, then the
/// tagma's configured timezone, then the machine's local zone. The
/// settings fetch is best-effort with its own short timeout; any failure
/// silently degrades to local.
pub(crate) async fn display_zone(utc: bool, client: &TagmaClient) -> DisplayZone {
    if utc {
        return DisplayZone::Utc;
    }
    match client.get_timezone().await {
        Ok(Some(name)) => DisplayZone::Named(name),
        _ => DisplayZone::Local,
    }
}

/// The time column for one list row: pick the axis field, then either
/// pass the ISO stamp through or shrink it to a relative distance. `-`
/// marks an absent axis value (an active task has no completion time).
fn axis_stamp(
    axis: TaskTimeAxisArg,
    relative: bool,
    zone: &DisplayZone,
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
        Some(iso) => match timefmt::parse_utc(iso) {
            Ok(epoch) => timefmt::format_display(epoch, zone),
            Err(_) => iso.to_string(),
        },
        None => "-".to_string(),
    }
}

pub async fn run_task(client: &TagmaClient, cmd: &TaskCommand) -> Result<()> {
    match cmd {
        TaskCommand::Create(args) => {
            let task = client
                .task_create(&TaskCreateRequest {
                    title: args.title.clone(),
                    assignee: args.assignee.clone(),
                    require: args.require.clone(),
                    dossier_path: args.dossier.as_ref().map(|p| p.display().to_string()),
                    inbox_id_start: args.inbox_start,
                    inbox_id_end: args.inbox_end,
                    room_id: args.room.clone(),
                    room_seq_start: args.room_seq_start,
                    room_seq_end: args.room_seq_end,
                })
                .await?;
            print_state_line(&task);
        }
        TaskCommand::Start(args) => {
            let task = client
                .task_start(args.id, &TaskForceRequest { force: args.force })
                .await?;
            print_state_line(&task);
        }
        TaskCommand::Confirm(args) => {
            let file = match &args.file {
                Some(path) => {
                    let body = std::fs::read_to_string(path)
                        .map_err(|e| anyhow!("read {}: {e}", path.display()))?;
                    let size = body.len();
                    if size > REPORT_MAX_BYTES {
                        return Err(anyhow!(
                            "report is {size} bytes; the cap is {REPORT_MAX_BYTES} bytes"
                        ));
                    }
                    Some(body)
                }
                None => None,
            };
            let task = client
                .task_confirm(
                    args.id,
                    &TaskConfirmRequest {
                        note: args.note.clone(),
                        file,
                    },
                )
                .await?;
            print_state_line(&task);
        }
        TaskCommand::Review(args) => {
            let task = client.task_review(args.id).await?;
            print_state_line(&task);
        }
        TaskCommand::Pause(args) => {
            let task = client.task_pause(args.id).await?;
            print_state_line(&task);
        }
        TaskCommand::Resume(args) => {
            let task = client
                .task_resume(args.id, &TaskForceRequest { force: args.force })
                .await?;
            print_state_line(&task);
        }
        TaskCommand::Close(args) => {
            let task = client
                .task_close(
                    args.id,
                    &TaskCloseRequest {
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
            let task = client
                .task_reopen(args.id, &TaskForceRequest { force: args.force })
                .await?;
            print_state_line(&task);
        }

        TaskCommand::Note(args) => {
            let task = client
                .task_note(
                    args.id,
                    &TaskNoteRequest {
                        note: args.note.clone(),
                    },
                )
                .await?;
            print_state_line(&task);
        }
        TaskCommand::Archive(args) => {
            let task = client
                .task_archive(args.id, &TaskForceRequest { force: args.force })
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
            let zone = display_zone(args.utc, client).await;
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
            println!("current datetime: {}", timefmt::format_display(now, &zone));
            let page = client.task_list(&query).await?;
            if page.rows.is_empty() {
                println!("(no tasks, {} total)", page.total);
            } else {
                for t in &page.rows {
                    let shown = axis_stamp(
                        args.time,
                        args.relative_time,
                        &zone,
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
                println!(
                    "{}",
                    task_list_footer(page.rows.len(), page.total, args.limit, args.offset)
                );
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
        TaskCommand::Report(args) => match &args.command {
            TaskReportCommand::List(args) => {
                let export = client.task_show(args.id).await?;
                let mut confirms = task_confirms(&export);
                if let Some(confirmer) = args.confirmer.as_deref() {
                    confirms.retain(|r| confirm_matches_confirmer(r, confirmer));
                }
                if confirms.is_empty() {
                    println!("(no confirmations filed)");
                    return Ok(());
                }
                for r in &confirms {
                    let size = match &r.report {
                        Some(body) => format!("{}B", body.len()),
                        None => "-".to_string(),
                    };
                    println!(
                        "{} v{} {} {size}",
                        r.confirmer,
                        r.version,
                        r.created_at.as_deref().unwrap_or("?")
                    );
                }
            }
            TaskReportCommand::Show(args) => {
                let export = client.task_show(args.id).await?;
                let confirms = task_confirms(&export);
                let mine: Vec<&TaskConfirmView> = confirms
                    .iter()
                    .filter(|r| confirm_matches_confirmer(r, args.confirmer.as_str()))
                    .collect();
                let picked = match args.version {
                    Some(v) => mine.iter().copied().find(|r| r.version == v),
                    None => mine.last().copied(),
                };
                let r = picked.ok_or_else(|| {
                    anyhow!(
                        "no matching confirmation for confirmer '{}' (try: kallip task report list)",
                        args.confirmer
                    )
                })?;
                let report = r.report.as_deref().ok_or_else(|| {
                    anyhow!(
                        "confirmation v{} for confirmer '{}' carries no report",
                        r.version,
                        r.confirmer
                    )
                })?;
                match &args.out {
                    Some(path) => std::fs::write(path, report)?,
                    None => print!("{report}"),
                }
            }
        },
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
    if !e.confirmers.is_empty() {
        println!("confirmers: {}", e.confirmers.join(", "));
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
            ev.actor_role
                .as_deref()
                .or(ev.actor.as_deref())
                .unwrap_or("-")
        );
    }
}

fn close_reason(reason: TaskCloseReason) -> ClosedReason {
    match reason {
        TaskCloseReason::Completed => ClosedReason::Completed,
        TaskCloseReason::NotPlanned => ClosedReason::NotPlanned,
        TaskCloseReason::Duplicate => ClosedReason::Duplicate,
    }
}
/// One filed confirmation, read back from the event trail: `task report`
/// filters this list locally instead of a dedicated endpoint. Versions
/// count per confirmer key (role name when resolved, else the raw actor)
/// in event id order, from 1.
struct TaskConfirmView {
    confirmer: String,
    /// The raw actor, kept so `--confirmer <agent id>` still matches
    /// events
    /// the server could not resolve to a role.
    actor: Option<String>,
    version: u32,
    created_at: Option<String>,
    /// The report body; `None` for a bare confirmation (no --file).
    report: Option<String>,
}

fn task_confirms(export: &TaskExport) -> Vec<TaskConfirmView> {
    let mut filed: Vec<(i64, TaskConfirmView)> = export
        .events
        .iter()
        .filter(|ev| ev.kind == "action" && ev.name == "confirm")
        .map(|ev| {
            let report = ev
                .payload
                .as_ref()
                .and_then(|p| p.get("file"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let view = TaskConfirmView {
                confirmer: ev
                    .actor_role
                    .clone()
                    .or_else(|| ev.actor.clone())
                    .unwrap_or_else(|| "-".to_string()),
                actor: ev.actor.clone(),
                version: 0,
                created_at: ev.created_at.clone(),
                report,
            };
            (ev.id, view)
        })
        .collect();
    filed.sort_by_key(|(id, _)| *id);
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for (_, view) in &mut filed {
        let next = seen.entry(view.confirmer.clone()).or_insert(0);
        *next += 1;
        view.version = *next;
    }
    filed.into_iter().map(|(_, view)| view).collect()
}

/// One-line footer for `task list`: paginated form under a positive
/// --limit, bare counts otherwise.
fn task_list_footer(showing: usize, total: i64, limit: Option<u32>, offset: Option<u32>) -> String {
    match limit {
        Some(limit) if limit > 0 => {
            let offset = u64::from(offset.unwrap_or(0));
            let limit = u64::from(limit);
            let page_no = offset / limit + 1;
            let pages = (total as u64).div_ceil(limit);
            format!("(showing {showing}, page {page_no} / {pages}, {total} total)")
        }
        _ => format!("(showing {showing}, {total} total)"),
    }
}

/// A confirmation belongs to `--confirmer` when its resolved confirmer
/// name matches, or when the raw actor matches literally (unresolved
/// actors).
fn confirm_matches_confirmer(confirm: &TaskConfirmView, confirmer: &str) -> bool {
    confirm.confirmer == confirmer || confirm.actor.as_deref() == Some(confirmer)
}

#[cfg(test)]
mod list_render_tests {
    use super::*;

    #[test]
    fn updated_axis_reads_updated_at_by_default() {
        let s = axis_stamp(
            TaskTimeAxisArg::Updated,
            false,
            &DisplayZone::Utc,
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
            &DisplayZone::Utc,
            100,
            Some("ISO-U"),
            Some("ISO-E"),
        );
        assert_eq!(s, "ISO-E");
    }

    #[test]
    fn absent_axis_value_renders_as_dash() {
        assert_eq!(
            axis_stamp(
                TaskTimeAxisArg::Closed,
                false,
                &DisplayZone::Utc,
                100,
                Some("ISO-U"),
                None,
            ),
            "-"
        );
    }

    #[test]
    fn relative_mode_shrinks_parsable_stamps_and_keeps_odd_ones() {
        let now = timefmt::now_epoch();
        let iso = timefmt::format_utc(now - 3 * 60 * 60);
        assert!(
            axis_stamp(
                TaskTimeAxisArg::Updated,
                true,
                &DisplayZone::Utc,
                now,
                Some(&iso),
                None
            )
            .starts_with("3h ago"),
        );
        assert_eq!(
            axis_stamp(
                TaskTimeAxisArg::Updated,
                true,
                &DisplayZone::Utc,
                now,
                Some("not-a-stamp"),
                None
            ),
            "not-a-stamp"
        );
    }

    /// Versions count per confirmer key (role name when the server resolved
    /// one, else the raw actor) in event id order, from 1.
    #[test]
    fn confirm_versions_count_per_confirmer_by_event_order() {
        let export = kallip_common::protocol::TaskExport {
            id: 1,
            title: "t".into(),
            status: "review".into(),
            creator: None,
            assignee: None,
            confirmers: vec![],
            created_at: None,
            updated_at: None,
            started_at: None,
            ended_at: None,
            archived: false,
            archived_at: None,
            closed_reason: None,
            close_summary: None,
            association: None,
            dossier_path: None,
            archive_hash: None,
            events: vec![
                confirm_event(1, "agent-1", Some("reviewer-c")),
                confirm_event(2, "agent-2", Some("reviewer-h")),
                confirm_event(3, "agent-1", Some("reviewer-c")),
            ],
        };
        let confirms = task_confirms(&export);
        let versions: Vec<(&str, u32)> = confirms
            .iter()
            .map(|r| (r.confirmer.as_str(), r.version))
            .collect();
        assert_eq!(
            versions,
            vec![("reviewer-c", 1), ("reviewer-h", 1), ("reviewer-c", 2)]
        );
    }

    /// Fixture: one confirm event; the role rides in actor_role when
    /// the server could resolve the actor to a registered agent.
    fn confirm_event(
        id: i64,
        actor: &str,
        role: Option<&str>,
    ) -> kallip_common::protocol::EventExport {
        kallip_common::protocol::EventExport {
            id,
            kind: "action".into(),
            name: "confirm".into(),
            actor: Some(actor.into()),
            actor_role: role.map(|r| r.into()),
            assignee: None,
            from_status: None,
            to_status: None,
            payload: None,
            created_at: None,
        }
    }

    #[test]
    fn footer_without_positive_limit_shows_bare_counts() {
        assert_eq!(task_list_footer(5, 42, None, None), "(showing 5, 42 total)");
        assert_eq!(
            task_list_footer(5, 42, Some(0), None),
            "(showing 5, 42 total)"
        );
    }

    #[test]
    fn footer_unaligned_offset_computes_page_number() {
        assert_eq!(
            task_list_footer(10, 45, Some(10), Some(15)),
            "(showing 10, page 2 / 5, 45 total)"
        );
    }

    #[test]
    fn confirmer_predicate_role_hit_actor_fallback_double_miss() {
        let view = TaskConfirmView {
            confirmer: "reviewer-c".into(),
            actor: Some("agent-1".into()),
            version: 1,
            created_at: None,
            report: None,
        };
        assert!(confirm_matches_confirmer(&view, "reviewer-c"));
        assert!(confirm_matches_confirmer(&view, "agent-1"));
        assert!(!confirm_matches_confirmer(&view, "agent-2"));
    }
}
