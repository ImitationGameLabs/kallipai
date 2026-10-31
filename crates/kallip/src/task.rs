//! The `kallip task` family: thin rendering over the task-domain API.
//! Everything rides the tagma HTTP face (`/tasks`); the tagma process is
//! the SOLE writer of tasks.sqlite. Write verbs carry no actor: the
//! tagma records the acting agent from the authenticated identity.

use std::io::Cursor;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use kallipai_client::TagmaClient;
use kallipai_common::protocol::{
    ClosedReason, DOSSIER_MAX_BYTES, REPORT_MAX_BYTES, TaskCloseRequest, TaskConfirmFile,
    TaskConfirmRequest, TaskCreateRequest, TaskExport, TaskForceRequest, TaskListQuery,
    TaskNoteRequest, TaskStatus, TaskUsageExport,
};

use crate::args::task::{
    TaskCloseReason, TaskCommand, TaskReportCommand, TaskTimeAxisArg, parse_time_anchor,
};
use kallipai_common::timefmt;
use kallipai_common::timefmt::DisplayZone;

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

/// The `--require` values, guarded locally before a request goes out. A value
/// that trims to empty is a caller mistake — a shell variable that expanded to
/// nothing — and the tagma reads a blank entry as "not given", so passing it
/// along would narrow the close gate silently. Non-blank values travel
/// unchanged for the tagma to resolve.
fn checked_roster(words: &[String]) -> Result<Vec<String>> {
    if words.iter().any(|w| w.trim().is_empty()) {
        return Err(anyhow!(
            "--require needs a role name, an agent id, or `operator`; an empty value is not a confirmer"
        ));
    }
    Ok(words.to_vec())
}

pub async fn run_task(client: &TagmaClient, cmd: &TaskCommand) -> Result<()> {
    match cmd {
        TaskCommand::Create(args) => {
            let task = client
                .task_create(&TaskCreateRequest {
                    title: args.title.clone(),
                    assignee: args.assignee.clone(),
                    require: checked_roster(&args.require)?,
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
                    Some(TaskConfirmFile {
                        file_name: path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "report.txt".to_string()),
                        file_b64: base64::engine::general_purpose::STANDARD.encode(body),
                    })
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
            // Pack at close time: the directory freezes here, in the
            // caller's shell, and travels as bytes. The tagma never
            // reads the filesystem to close a task.
            let dossier_tar_b64 = match &args.dossier {
                Some(dir) => {
                    if !dir.is_dir() {
                        return Err(anyhow!("dossier {} is not a directory", dir.display()));
                    }
                    let packed = kallipai_blob_store::archive::pack_dir(
                        dir,
                        Some(DOSSIER_MAX_BYTES as u64),
                    )?;
                    Some(base64::engine::general_purpose::STANDARD.encode(packed))
                }
                None => None,
            };
            let task = client
                .task_close(
                    args.id,
                    &TaskCloseRequest {
                        reason: close_reason(args.reason),
                        summary: args.summary.clone(),
                        force: args.force,
                        dossier_tar_b64,
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
                        shown_identity(t.assignee.as_deref(), t.assignee_role.as_deref()),
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
                    let attached = match &r.report {
                        Some(file) => format!("{} {}B", file.file_name, file.size),
                        None => "-".to_string(),
                    };
                    println!(
                        "{} v{} {} {attached}",
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
                let pointer = r.report.as_ref().ok_or_else(|| {
                    anyhow!(
                        "confirmation v{} for confirmer '{}' carries no report",
                        r.version,
                        r.confirmer
                    )
                })?;
                let report = client.task_report_body(args.id, &pointer.blob).await?;
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

/// The display rule the JSON face spells out in two fields: the role name
/// when the ledger's identity resolves to one, the identity otherwise, and
/// a dash when the task has none.
fn shown_identity<'a>(raw: Option<&'a str>, role: Option<&'a str>) -> &'a str {
    role.or(raw).unwrap_or("-")
}

/// The confirmer roster as one line: every identity through `shown_identity`,
/// aligned by index with `confirmer_roles`, so a missing or null role entry
/// falls back to the identity the ledger stores.
fn shown_roster(export: &TaskExport) -> String {
    let shown: Vec<&str> = export
        .confirmers
        .iter()
        .enumerate()
        .map(|(index, identity)| {
            shown_identity(
                Some(identity.as_str()),
                export
                    .confirmer_roles
                    .get(index)
                    .and_then(|role| role.as_deref()),
            )
        })
        .collect();
    shown.join(", ")
}

fn print_show(e: &TaskExport) {
    println!("task {}: '{}'", e.id, e.title);
    println!(
        "status: {}  assignee: {}  creator: {}",
        e.status,
        shown_identity(e.assignee.as_deref(), e.assignee_role.as_deref()),
        shown_identity(e.creator.as_deref(), e.creator_role.as_deref())
    );
    if !e.confirmers.is_empty() {
        println!("confirmers: {}", shown_roster(e));
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
    if let Some(hash) = &e.archive_hash {
        match e.archive_entries {
            Some(entries) => println!("archive: {hash} (closed, {entries} entries)"),
            None => println!("archive: {hash} (closed)"),
        }
    }
    if let Some(reason) = &e.closed_reason {
        match &e.close_summary {
            Some(summary) => println!("closed: {reason} — {summary}"),
            None => println!("closed: {reason}"),
        }
    }
    print_usage(e.usage.as_ref());
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
            shown_identity(ev.actor.as_deref(), ev.actor_role.as_deref())
        );
    }
}

/// Print the token-accounting line: what the task's active windows cost,
/// with the caveat the number needs. A task that predates the snapshots
/// says so instead of reporting a zero.
fn print_usage(usage: Option<&TaskUsageExport>) {
    match usage {
        Some(usage) => println!("{}", usage_line(usage)),
        None => println!("usage: not recorded"),
    }
}

/// The line itself, split out from the printing so the shapes it takes
/// (no window, a single token or gap, a partial task) stay testable.
fn usage_line(usage: &TaskUsageExport) -> String {
    let windows = usage.segments.len();
    // The window clause is dropped when there is no window: "0 active
    // windows" carries nothing a reader of a running task needs.
    let mut line = if windows == 0 {
        format!(
            "usage: {} {}",
            usage.total_tokens,
            plural(usage.total_tokens, "token", "tokens")
        )
    } else {
        format!(
            "usage: {} {} over {} active {}",
            usage.total_tokens,
            plural(usage.total_tokens, "token", "tokens"),
            windows,
            plural(windows as u64, "window", "windows"),
        )
    };
    let per_agent: Vec<String> = usage
        .agents
        .iter()
        .map(|agent| {
            let name = shown_identity(Some(agent.agent_id.as_str()), agent.agent_role.as_deref());
            format!("{name} {}", agent.consumed_tokens)
        })
        .collect();
    if !per_agent.is_empty() {
        line.push_str(&format!(" ({})", per_agent.join(", ")));
    }
    if usage.partial {
        // `partial` also covers incompleteness that leaves no gap rows (a
        // failed capture, an unreadable snapshot), so the count is attached
        // only when there is one.
        match usage.gaps.len() {
            0 => line.push_str(", partial"),
            gaps => line.push_str(&format!(
                ", partial ({gaps} {})",
                plural(gaps as u64, "gap", "gaps")
            )),
        }
    }
    if let Some(since) = &usage.open_since {
        line.push_str(&format!(", window open since {since}"));
    }
    line
}

/// `1 token` / `2 tokens`: the unit that goes with a count.
fn plural<'a>(count: u64, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
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
    /// The stored report pointer; `None` for a bare confirmation
    /// (no --file).
    report: Option<ReportPointer>,
}

/// One stored report: the content address the body is fetched by, plus
/// the display metadata recorded at confirm time.
struct ReportPointer {
    blob: String,
    file_name: String,
    size: i64,
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
                .and_then(|v| {
                    Some(ReportPointer {
                        blob: v.get("file_blob")?.as_str()?.to_string(),
                        file_name: v.get("file_name")?.as_str()?.to_string(),
                        size: v.get("size")?.as_i64()?,
                    })
                });
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
        let export = kallipai_common::protocol::TaskExport {
            id: 1,
            title: "t".into(),
            status: "review".into(),
            creator: None,
            assignee: None,
            confirmers: vec![],
            creator_role: None,
            assignee_role: None,
            confirmer_roles: vec![],
            created_at: None,
            updated_at: None,
            started_at: None,
            ended_at: None,
            archived: false,
            archived_at: None,
            closed_reason: None,
            close_summary: None,
            association: None,
            archive_hash: None,
            archive_entries: None,
            usage: None,
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
    ) -> kallipai_common::protocol::EventExport {
        kallipai_common::protocol::EventExport {
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

#[cfg(test)]
mod usage_line_tests {
    use super::*;
    use kallipai_common::protocol::{
        UsageAgentExport, UsageGapExport, UsageGapReason, UsageSegmentExport,
    };

    fn accounted(total_tokens: u64) -> TaskUsageExport {
        TaskUsageExport {
            total_tokens,
            open: false,
            open_since: None,
            segments: Vec::new(),
            agents: Vec::new(),
            gaps: Vec::new(),
            partial: false,
            non_monotonic: Vec::new(),
        }
    }

    fn segment(from: &str, to: &str) -> UsageSegmentExport {
        UsageSegmentExport {
            from_event: from.to_string(),
            to_event: to.to_string(),
            from_at: "2025-10-09T08:53:20Z".to_string(),
            to_at: "2025-10-09T09:53:20Z".to_string(),
            total_tokens: 10,
            agents: Vec::new(),
        }
    }

    fn gap(agent_id: &str) -> UsageGapExport {
        UsageGapExport {
            agent_id: agent_id.to_string(),
            segment: 1,
            reason: UsageGapReason::MissingTerminal,
        }
    }

    #[test]
    fn a_single_token_stays_singular() {
        assert_eq!(usage_line(&accounted(1)), "usage: 1 token");
    }

    #[test]
    fn a_task_without_a_closed_window_drops_the_window_clause() {
        let mut usage = accounted(0);
        usage.open = true;
        usage.open_since = Some("2025-10-09T08:53:20Z".to_string());
        assert_eq!(
            usage_line(&usage),
            "usage: 0 tokens, window open since 2025-10-09T08:53:20Z"
        );
    }

    #[test]
    fn windows_and_gaps_read_in_the_singular_and_the_plural() {
        let mut usage = accounted(1_000);
        usage.segments = vec![segment("start", "close")];
        assert_eq!(
            usage_line(&usage),
            "usage: 1000 tokens over 1 active window"
        );
        usage.segments.push(segment("resume", "close"));
        assert_eq!(
            usage_line(&usage),
            "usage: 1000 tokens over 2 active windows"
        );
        usage.partial = true;
        assert_eq!(
            usage_line(&usage),
            "usage: 1000 tokens over 2 active windows, partial"
        );
        usage.gaps = vec![gap("a")];
        assert_eq!(
            usage_line(&usage),
            "usage: 1000 tokens over 2 active windows, partial (1 gap)"
        );
        usage.gaps.push(gap("b"));
        assert_eq!(
            usage_line(&usage),
            "usage: 1000 tokens over 2 active windows, partial (2 gaps)"
        );
    }

    #[test]
    fn agent_rows_prefer_the_role_and_fall_back_to_the_id() {
        let mut usage = accounted(30);
        usage.agents = vec![
            UsageAgentExport {
                agent_id: "11111111-1111-1111-1111-111111111111".to_string(),
                agent_role: Some("dev".to_string()),
                consumed_tokens: 20,
                prompt_tokens: 20,
                completion_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            UsageAgentExport {
                agent_id: "agent-b".to_string(),
                agent_role: None,
                consumed_tokens: 10,
                prompt_tokens: 10,
                completion_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        ];
        assert_eq!(usage_line(&usage), "usage: 30 tokens (dev 20, agent-b 10)");
    }

    #[test]
    fn a_task_with_no_window_but_with_gaps_reads_as_partial() {
        // The shape a pre-feature task takes when only its terminal carried
        // a snapshot: nothing to sum, and the agents it named are gaps.
        let mut usage = accounted(0);
        usage.partial = true;
        usage.gaps = vec![gap("a")];
        assert_eq!(usage_line(&usage), "usage: 0 tokens, partial (1 gap)");
    }

    #[test]
    fn the_confirmer_line_reads_roles_and_falls_back_by_index() {
        // One roster entry has a role name, one resolves to nothing, and one is
        // the operator: each cell follows the same rule as the assignee column,
        // and an index past the end of `confirmer_roles` is not an off-by-one.
        let export = kallipai_common::protocol::TaskExport {
            id: 1,
            title: "t".into(),
            status: "review".into(),
            creator: None,
            assignee: None,
            confirmers: vec!["agent-1".into(), "agent-2".into(), "operator".into()],
            creator_role: None,
            assignee_role: None,
            confirmer_roles: vec![Some("reviewer-c".into()), None],
            created_at: None,
            updated_at: None,
            started_at: None,
            ended_at: None,
            archived: false,
            archived_at: None,
            closed_reason: None,
            close_summary: None,
            association: None,
            archive_hash: None,
            archive_entries: None,
            usage: None,
            events: vec![],
        };
        assert_eq!(
            shown_roster(&export),
            "reviewer-c, agent-2, operator",
            "role names where the registry has them, the identity otherwise"
        );
    }

    #[test]
    fn the_assignee_column_shows_the_role_and_falls_back_to_the_id() {
        // The ledger stores identities; a reader wants names. A name the
        // registry cannot supply is not invented, and an unassigned task
        // reads as a dash.
        assert_eq!(
            shown_identity(Some("0e70ad8c-5340-41ba-b106-ea0050d86daf"), Some("dev")),
            "dev"
        );
        assert_eq!(
            shown_identity(Some("0e70ad8c-5340-41ba-b106-ea0050d86daf"), None),
            "0e70ad8c-5340-41ba-b106-ea0050d86daf"
        );
        assert_eq!(shown_identity(None, None), "-");
    }
}

#[cfg(test)]
mod roster_guard_tests {
    use super::*;

    /// A blank `--require` value is a caller mistake, not an omission: the
    /// tagma would drop it as "not given" and narrow the close gate silently,
    /// so the CLI refuses before a request is built.
    #[test]
    fn a_blank_require_value_is_refused_before_the_request() {
        let err = checked_roster(&["dev".to_string(), "  ".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("--require needs a role name"),
            "{err}"
        );
        assert!(checked_roster(&[]).unwrap().is_empty());
        assert_eq!(
            checked_roster(&["operator".to_string()]).unwrap(),
            vec!["operator".to_string()]
        );
    }
}
