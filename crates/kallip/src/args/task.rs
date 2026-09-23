//! `task` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand, ValueEnum};
use std::path::PathBuf;

/// The CLI mirror of the wire's time axis: which timestamp the window,
/// the sort, and the time column anchor to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum TaskTimeAxisArg {
    /// Last activity time (the default).
    Updated,
    /// Completion time.
    Closed,
}

impl From<TaskTimeAxisArg> for kallip_common::protocol::TaskTimeAxis {
    fn from(a: TaskTimeAxisArg) -> Self {
        match a {
            TaskTimeAxisArg::Updated => kallip_common::protocol::TaskTimeAxis::Updated,
            TaskTimeAxisArg::Closed => kallip_common::protocol::TaskTimeAxis::Closed,
        }
    }
}

/// Parses a --since/--until value: relative days (`3d` = now minus 3
/// days) or an absolute UTC date (`2026-09-15` = that day, 00:00:00Z).
/// Returns epoch seconds.
pub(crate) fn parse_time_anchor(raw: &str, now_secs: u64) -> anyhow::Result<u64> {
    let raw = raw.trim();
    if let Some(days) = raw.strip_suffix('d') {
        let days: u64 = days.parse().map_err(|_| {
            anyhow::anyhow!("bad --since/--until value '{raw}' (try 3d or 2026-09-15)")
        })?;
        return Ok(now_secs.saturating_sub(days.saturating_mul(24 * 60 * 60)));
    }
    kallip_common::timefmt::parse_utc_day(raw)
        .map_err(|_| anyhow::anyhow!("bad --since/--until value '{raw}' (try 3d or 2026-09-15)"))
}

// ---------------------------------------------------------------------------
// Task commands — the tagma's task ledger, over the task domain API
// ---------------------------------------------------------------------------

/// The tagma's task ledger: queue, coarse state machine, event trail, hard
/// gates, and closed-task archives. Verbs go through the tagma task API
/// (the CLI never touches tasks.sqlite); the acting agent is taken
/// from the authenticated identity. Write verbs are enforced server-side:
/// the CLI is an entry point, the tagma is the law.
#[derive(Subcommand)]
pub(crate) enum TaskCommand {
    /// Register a task in the queue: title, optional assignee, the
    /// confirmer roster (--require, repeatable), dossier, association keys.
    Create(TaskCreateArgs),
    /// Pick a queued task up (queued -> in_progress; the serial gate
    /// applies, --force to override with an auditable escape).
    Start(TaskStartArgs),
    /// File a confirmation toward the close gate (optionally carrying a
    /// review report via --file).
    Confirm(TaskConfirmArgs),
    /// Move the task in_progress -> review.
    Review(TaskReviewArgs),
    /// Move the task in_progress -> paused (no gate).
    Pause(TaskPauseArgs),
    /// Move the task paused -> in_progress (the serial gate applies,
    /// --force to override with an auditable escape).
    Resume(TaskResumeArgs),
    /// Close a task: every registered confirmer must have filed a
    /// confirmation, or pass --force with an auditable escape. The
    /// dossier (if registered) is packed canonically and
    /// content-addressed on close.
    Close(TaskCloseArgs),
    /// Reopen a closed task (back to in_progress).
    Reopen(TaskReopenArgs),
    /// Append a note to the task's trail (never moves the machine).
    Note(TaskNoteArgs),
    /// Archive a closed task: it leaves the default list view
    /// (`task list --archived` shows archived tasks).
    Archive(TaskArchiveArgs),
    /// List tasks (newest first on the chosen time axis).
    /// --since/--until anchor to --time; --limit/--offset page
    /// server-side; --relative-time switches the time column to
    /// relative distances.
    List(TaskListArgs),
    /// Show one task: current state, association keys, event trail.
    Show(TaskShowArgs),
    /// Export a task (id, or all tasks with --all). Default renders the
    /// show view; --json emits the machine face (stable field names, ISO
    /// 8601 UTC times) including the full event trail.
    Export(TaskExportArgs),
    /// Extract a closed task's content-addressed archive (the dossier
    /// snapshot frozen at close) into a directory.
    Extract(TaskExtractArgs),
    /// Review confirmations for a task: list what was filed, or show
    /// one confirmation's note and report (pick the confirmer and
    /// version).
    Report(TaskReportArgs),
}

#[derive(Args)]
pub(crate) struct TaskStartArgs {
    /// Existing task id to pick up (queued -> in_progress).
    pub id: i64,
    /// Override the serial gate (the escape is recorded in the event trail).
    #[arg(long)]
    pub force: bool,
}
#[derive(Args)]
pub(crate) struct TaskConfirmArgs {
    /// Task id (must be in_progress or review).
    pub id: i64,
    /// Confirmation note.
    #[arg(long)]
    pub note: Option<String>,
    /// Read the review report body from this UTF-8 file.
    #[arg(long)]
    pub file: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct TaskReviewArgs {
    /// Task id.
    pub id: i64,
}

#[derive(Args)]
pub(crate) struct TaskPauseArgs {
    /// Task id.
    pub id: i64,
}

#[derive(Args)]
pub(crate) struct TaskResumeArgs {
    /// Task id.
    pub id: i64,
    /// Override the serial gate (the escape is recorded in the event trail).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub(crate) struct TaskCreateArgs {
    /// Title for the new task.
    #[arg(long)]
    pub title: String,
    /// Assignee for the new task; defaults to whoever picks it up.
    #[arg(long)]
    pub assignee: Option<String>,
    /// Register a confirmer (repeatable). Every registered confirmer must
    /// file a confirmation before the task can close.
    #[arg(long = "require")]
    pub require: Vec<String>,
    /// Dossier directory packed into the closed archive on close.
    #[arg(long)]
    pub dossier: Option<PathBuf>,
    /// Association key: inbox id window start (the task's message trail).
    #[arg(long)]
    pub inbox_start: Option<i64>,
    /// Association key: inbox id window end.
    #[arg(long)]
    pub inbox_end: Option<i64>,
    /// Association key: lesche room id.
    #[arg(long)]
    pub room: Option<String>,
    /// Association key: room seq window start.
    #[arg(long)]
    pub room_seq_start: Option<i64>,
    /// Association key: room seq window end.
    #[arg(long)]
    pub room_seq_end: Option<i64>,
}

#[derive(Args)]
pub(crate) struct TaskCloseArgs {
    /// Task id.
    pub id: i64,
    /// Why the task closes (gh-style two-level terminal state).
    #[arg(long, value_enum, default_value_t = TaskCloseReason::Completed)]
    pub reason: TaskCloseReason,
    /// One-sentence result recorded with the close event.
    #[arg(long)]
    pub summary: Option<String>,
    /// Override the confirmation gate (the escape is recorded in the event
    /// trail).
    #[arg(long)]
    pub force: bool,
}

/// Terminal-state reason, serialized snake_case on the store face.
#[derive(clap::ValueEnum, Clone, Copy)]
#[value(rename_all = "snake_case")]
pub(crate) enum TaskCloseReason {
    Completed,
    NotPlanned,
    Duplicate,
}

#[derive(Args)]
pub(crate) struct TaskReopenArgs {
    /// Task id.
    pub id: i64,
    /// Override the serial gate (the escape is recorded in the event trail).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub(crate) struct TaskNoteArgs {
    /// Task id (any state, closed included).
    pub id: i64,
    /// The note to append.
    #[arg(long)]
    pub note: String,
}

#[derive(Args)]
pub(crate) struct TaskArchiveArgs {
    /// Task id (must be closed).
    pub id: i64,
    /// Override the closed-only gate (the escape is recorded).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub(crate) struct TaskListArgs {
    /// Filter by status: queued, in_progress, paused, review, closed.
    #[arg(long)]
    pub status: Option<String>,
    /// Filter by assignee.
    #[arg(long)]
    pub assignee: Option<String>,
    /// List archived tasks only (the default view is the active one).
    #[arg(long)]
    pub archived: bool,
    /// The time axis: updated (last activity, the default) or closed
    /// (completion time). The --since/--until window, the sort order,
    /// and the time column all anchor to it.
    #[arg(long, value_enum, default_value_t = TaskTimeAxisArg::Updated)]
    pub time: TaskTimeAxisArg,
    /// Window start on the axis: days back (3d) or a UTC date
    /// (2026-09-15).
    #[arg(long)]
    pub since: Option<String>,
    /// Window end on the axis, same forms as --since.
    #[arg(long)]
    pub until: Option<String>,
    /// Page size.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Page start.
    #[arg(long)]
    pub offset: Option<u32>,
    /// Show the time column as relative distances (3h ago) instead of
    /// absolute stamps.
    #[arg(long)]
    pub relative_time: bool,
    /// Render absolute stamps in UTC instead of the tagma's configured
    /// timezone or the machine's local zone.
    #[arg(long)]
    pub utc: bool,
}

#[derive(Args)]
pub(crate) struct TaskShowArgs {
    /// Task id.
    pub id: i64,
}

#[derive(Args)]
pub(crate) struct TaskExportArgs {
    /// Task id; omit with --all.
    pub id: Option<i64>,
    /// Export every task.
    #[arg(long)]
    pub all: bool,
    /// Emit the machine face (JSON) instead of the show view.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub(crate) struct TaskExtractArgs {
    /// Task id.
    pub id: i64,
    /// Destination directory (created if absent).
    #[arg(long)]
    pub to: PathBuf,
}

#[derive(Args)]
pub(crate) struct TaskReportArgs {
    #[command(subcommand)]
    pub command: TaskReportCommand,
}

/// Review confirmations filed on a task, read back from the event trail.
#[derive(Subcommand)]
pub(crate) enum TaskReportCommand {
    /// List the confirmations filed on a task (confirmer, version, size).
    List(TaskReportListArgs),
    /// Show one confirmation: the confirmer, version, note, and report body.
    Show(TaskReportShowArgs),
}

#[derive(Args)]
pub(crate) struct TaskReportListArgs {
    /// Task id.
    pub id: i64,
    /// Filter to one confirmer (agent id or role name).
    #[arg(long)]
    pub confirmer: Option<String>,
    /// Write the listing to this file instead of stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct TaskReportShowArgs {
    /// Task id.
    pub id: i64,
    /// The confirmer whose confirmation to show (agent id or role name).
    #[arg(long)]
    pub confirmer: String,
    /// Confirmation version for that confirmer, 1-based (defaults to the
    /// latest).
    #[arg(long)]
    pub version: Option<u32>,
    /// Write the confirmation to this file instead of stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[cfg(test)]
mod window_tests {
    use super::parse_time_anchor;
    use kallip_common::timefmt;

    #[test]
    fn relative_days_anchored_to_now() {
        let now = timefmt::now_epoch();
        assert_eq!(
            parse_time_anchor("3d", now).unwrap(),
            now - 3 * 24 * 60 * 60
        );
        assert_eq!(parse_time_anchor(" 0d ", now).unwrap(), now);
    }

    #[test]
    fn absolute_date_is_that_days_utc_midnight() {
        let epoch = parse_time_anchor("2026-09-15", 0).unwrap();
        assert_eq!(timefmt::format_utc(epoch), "2026-09-15T00:00:00Z");
    }

    #[test]
    fn garbage_is_rejected_with_a_hint() {
        let err = parse_time_anchor("yesterday", 1_000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("3d or 2026-09-15"), "{err}");
        assert!(parse_time_anchor("12x", 1_000).is_err());
    }
}
