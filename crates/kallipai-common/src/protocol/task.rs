//! Task-domain wire types: the request bodies, the export shape, and the
//! state vocabulary shared by the task store, the tagma task routes, and
//! the CLI's task client.
//!
//! One definition per shape: the store's verb signatures, the HTTP
//! bodies, and the client methods all speak these types directly, so
//! all three faces bind to one definition and cannot drift apart.

use serde::{Deserialize, Serialize};
/// Upper bound for a review report, counted in decoded bytes. Shared by
/// the CLI (early, friendly failure) and the store (authoritative check).
/// The body lands in the content-addressed blob store and the confirm
/// event carries only the pointer, so the cap is a semantic anti-footgun
/// bound (a mistyped --file), not a database-size guard.
pub const REPORT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound for a closed-task dossier, counted in packed tar bytes.
/// Like [`REPORT_MAX_BYTES`] this is an anti-footgun bound — a `--dossier`
/// aimed at a giant tree fails the pack early instead of shipping a
/// giant request. The store aborts the pack mid-walk (early stop).
pub const DOSSIER_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Route-level body-limit override for the two task routes that carry
/// bulk payloads (`close` with a dossier, `confirm` with a report). The
/// tagma-wide default (`KALLIPAI_MAX_BODY_SIZE_KB`, 1 MiB) would 413 the
/// base64 body before the handler could give its friendly error; the
/// override stays scoped to these routes so no other endpoint loosens.
/// Sized for [`DOSSIER_MAX_BYTES`] base64-encoded (~85 MiB) with headroom.
pub const TASK_BODY_LIMIT_BYTES: usize = 96 * 1024 * 1024;

/// The five coarse states. Serialized lowercase snake_case on the wire,
/// matching [`TaskStatus::as_str`] and the SQLite spelling — the CLI's
/// `--status` vocabulary and the wire vocabulary are the same strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    InProgress,
    Paused,
    Review,
    Closed,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Paused => "paused",
            TaskStatus::Review => "review",
            TaskStatus::Closed => "closed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "queued" => Some(TaskStatus::Queued),
            "in_progress" => Some(TaskStatus::InProgress),
            "paused" => Some(TaskStatus::Paused),
            "review" => Some(TaskStatus::Review),
            "closed" => Some(TaskStatus::Closed),
            _ => None,
        }
    }
}

/// Why a task was closed. An attribute of `closed`, not a state of its
/// own. Wire spelling matches [`ClosedReason::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosedReason {
    Completed,
    NotPlanned,
    Duplicate,
}

impl ClosedReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClosedReason::Completed => "completed",
            ClosedReason::NotPlanned => "not_planned",
            ClosedReason::Duplicate => "duplicate",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "completed" => Some(ClosedReason::Completed),
            "not_planned" => Some(ClosedReason::NotPlanned),
            "duplicate" => Some(ClosedReason::Duplicate),
            _ => None,
        }
    }
}

/// Body of `POST /tasks`. Every field except the title is optional: a
/// minimal registration is a titled task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCreateRequest {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// The confirmer roster. Names are resolved to identities at
    /// registration; an empty list means closing needs no confirmations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inbox_id_start: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inbox_id_end: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_seq_start: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_seq_end: Option<i64>,
}

/// Body of `POST /tasks/{id}/confirm`. A confirmation records the
/// actor's sign-off toward the close gate, optionally carrying a
/// review report via `file`, size-capped at [`REPORT_MAX_BYTES`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskConfirmRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<TaskConfirmFile>,
}

/// Body of the force-carrying verbs: `start`, `resume`, `reopen`, and `archive`.
/// The flag is the auditable escape from the verb's gate; it is
/// recorded in the task's event trail.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskForceRequest {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

/// Body of `POST /tasks/{id}/note`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNoteRequest {
    pub note: String,
}

/// Body of `POST /tasks/{id}/close`. Closing requires a reason; the
/// confirmation gate counts registered confirmers against filed confirmations.
/// The dossier, when the caller names one, travels as canonical tar bytes
/// base64-encoded: the caller packs and freezes the content, the tagma
/// never touches the filesystem to close.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCloseRequest {
    pub reason: ClosedReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dossier_tar_b64: Option<String>,
}

/// A review report riding a confirmation: the file's name for display
/// and the UTF-8 body base64-encoded. The store ingests the body into
/// the blob store and the event carries only the pointer + metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfirmFile {
    pub file_name: String,
    pub file_b64: String,
}

/// Query parameters of `GET /tasks`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskListQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// The archive partition: false (default) lists active tasks only,
    /// true lists archived tasks only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    /// The time axis the since/until window anchors to and the list sorts
    /// by (and the time column shows). `updated` = last activity;
    /// `closed` = completion time. Orthogonal to --status: choosing the
    /// axis filters nothing by itself.
    #[serde(default)]
    pub time: TaskTimeAxis,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

/// The time axis for the task list's window, sort, and time column.
/// `updated` (default) = last activity; `closed` = completion time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskTimeAxis {
    #[default]
    #[serde(rename = "updated")]
    Updated,
    #[serde(rename = "closed")]
    Closed,
}

/// Association keys (K8s involvedObject shape): message windows the
/// task was cut from, recorded at create time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssociationExport {
    pub inbox_id_start: Option<i64>,
    pub inbox_id_end: Option<i64>,
    pub room_id: Option<String>,
    pub room_seq_start: Option<i64>,
    pub room_seq_end: Option<i64>,
}

#[derive(Serialize, Deserialize)]
pub struct EventExport {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub actor: Option<String>,
    /// Role name resolved from `actor` against the registry at export time;
    /// null when unresolvable (deregistered agent, historical confirmer-name
    /// actors, the literal `operator`, or a null actor).
    pub actor_role: Option<String>,
    pub assignee: Option<String>,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub payload: Option<serde_json::Value>,
    pub created_at: Option<String>,
}

/// One task plus its event trail, shaped for the machine face (export).
#[derive(Serialize, Deserialize)]
pub struct TaskExport {
    pub id: i64,
    pub title: String,
    pub status: String,
    pub creator: Option<String>,
    pub assignee: Option<String>,
    pub confirmers: Vec<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    /// The archive partition marker (a query partition, not a state).
    pub archived: bool,
    pub archived_at: Option<String>,
    pub closed_reason: Option<String>,
    pub close_summary: Option<String>,
    pub association: Option<AssociationExport>,
    /// The closed archive: the content address plus the tar entry count
    /// frozen at close. Absent until the task closes with a dossier.
    pub archive_hash: Option<String>,
    pub archive_entries: Option<i64>,
    /// Token accounting over the task's active windows, derived at read time
    /// from the per-transition snapshots in the trail. Absent when no
    /// transition carries one (tasks that predate the feature).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TaskUsageExport>,
    pub events: Vec<EventExport>,
}

/// Token accounting for one task, summed over its **active windows**.
///
/// A window is a `[baseline, terminal)` pair: the baseline is a `start`,
/// `resume`, or `reopen` transition, the terminal a `pause` or `close`.
/// Each counter delta is the terminal's value minus its baseline's, clamped
/// at zero; a window that cannot be paired is left out of the sum. An agent
/// that at least one of its ends did not read is listed in `gaps` with
/// `partial` set, while a window whose edges both recorded nothing sets
/// `partial` alone. The window still open — the one `open` and `open_since`
/// report — is not one of those: its agents never enter `gaps`.
///
/// The total covers the task's active windows only. Work the same agent did
/// elsewhere while a window was open (another task, unrelated turns) still
/// lands in these numbers, so `total_tokens` is not a per-task invoice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskUsageExport {
    /// Sum of the paired windows' consumed (prompt + completion) tokens.
    pub total_tokens: u64,
    /// The task holds an open window: its terminal has not happened yet,
    /// so its spend is not part of `total_tokens`. Normal for a running
    /// task, not a data gap.
    pub open: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_since: Option<String>,
    /// The windows that could be summed, in trail order. A window that is
    /// still open, or whose edges are incomplete, contributes none.
    pub segments: Vec<UsageSegmentExport>,
    /// Per-agent sums across every paired window.
    pub agents: Vec<UsageAgentExport>,
    /// Agents left out of the sum because an end did not read them.
    pub gaps: Vec<UsageGapExport>,
    /// A window could not be fully accounted: a gap, a failed capture, a
    /// snapshot that no longer parses, an edge that recorded nothing, or
    /// an agent a capture had to skip.
    pub partial: bool,
    /// Agents whose counters went backwards between two snapshots; their
    /// negative deltas were clamped to zero.
    pub non_monotonic: Vec<String>,
}

/// One summed window: `[from_event, to_event)`.
///
/// Only windows with both edges readable become segments; a window that
/// never closed, or whose other edge is missing or unreadable, is
/// accounted for by `open`, `gaps`, or `partial` instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSegmentExport {
    /// `start` | `resume` | `reopen`.
    pub from_event: String,
    /// `pause` | `close`.
    pub to_event: String,
    pub from_at: String,
    pub to_at: String,
    pub total_tokens: u64,
    pub agents: Vec<UsageAgentExport>,
}

/// One agent's counters over a window, or summed over every window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageAgentExport {
    pub agent_id: String,
    /// The agent's role name at read time, filled by the route the same way
    /// event actors are; null for an id that no longer resolves.
    pub agent_role: Option<String>,
    /// prompt + completion: the headline number.
    pub consumed_tokens: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// An agent left out of a sum, and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageGapExport {
    pub agent_id: String,
    /// 1-based ordinal of the window in trail order. Not an index into
    /// `segments`: a window whose edges are incomplete consumes an ordinal
    /// without contributing one, so the two can differ in length.
    pub segment: usize,
    pub reason: UsageGapReason,
}

/// Why a window could not account for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageGapReason {
    /// The baseline snapshot does not carry the agent: it was spawned
    /// mid-window, the baseline predates the feature, or its capture could
    /// not read the agent.
    MissingBaseline,
    /// The terminal snapshot does not carry the agent: it was removed,
    /// parked where its manifest could not be read, or its capture could
    /// not read the agent (a capture that ran out of budget lands here too).
    MissingTerminal,
}

/// The list-face row: a task without its event trail. `GET /tasks` returns
/// these under the paging envelope — the trail and the report bodies live
/// behind the per-task detail endpoint, so a list poll never drags them
/// back. `has_reports` marks tasks carrying at least one confirmation
/// with a report file attached.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: i64,
    pub title: String,
    pub status: String,
    pub creator: Option<String>,
    pub assignee: Option<String>,
    pub confirmers: Vec<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    /// The archive partition marker (a query partition, not a state).
    pub archived: bool,
    pub archived_at: Option<String>,
    pub closed_reason: Option<String>,
    pub close_summary: Option<String>,
    pub association: Option<AssociationExport>,
    /// The closed archive: content address plus tar entry count.
    pub archive_hash: Option<String>,
    pub archive_entries: Option<i64>,
    /// The task has at least one confirm event carrying a report file.
    pub has_reports: bool,
}

/// Paging envelope of `GET /tasks`: one page of lightweight rows plus the
/// total count of rows matching the filter (limit/offset excluded from
/// the count).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskListPage {
    pub rows: Vec<TaskRow>,
    pub total: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_wire_spelling_matches_as_str() {
        for raw in ["queued", "in_progress", "review", "paused", "closed"] {
            let parsed = TaskStatus::parse(raw).expect("vocabulary round-trips");
            assert_eq!(parsed.as_str(), raw);
            assert_eq!(
                serde_json::to_string(&parsed).unwrap(),
                format!("\"{raw}\"")
            );
        }
        assert_eq!(TaskStatus::parse("Queued"), None);
        assert_eq!(TaskStatus::parse("closed "), None);
    }
    #[test]
    fn confirm_request_file_skips_when_absent() {
        let bare = TaskConfirmRequest {
            note: None,
            file: None,
        };
        assert!(serde_json::to_value(&bare).unwrap().get("file").is_none());
        let carried = TaskConfirmRequest {
            file: Some(TaskConfirmFile {
                file_name: "review.md".into(),
                file_b64: "YXBwcm92ZWQ=".into(),
            }),
            ..Default::default()
        };
        let json = serde_json::to_value(&carried).unwrap();
        assert_eq!(json["file"]["file_name"], "review.md");
        assert_eq!(json["file"]["file_b64"], "YXBwcm92ZWQ=");
    }

    #[test]
    fn close_request_skips_an_absent_dossier() {
        let bare: TaskCloseRequest = serde_json::from_str(r#"{"reason":"completed"}"#).unwrap();
        assert!(bare.dossier_tar_b64.is_none());
        let json = serde_json::to_value(&bare).unwrap();
        assert!(json.get("dossier_tar_b64").is_none());
    }

    #[test]
    fn closed_reason_wire_spelling_matches_as_str() {
        for raw in ["completed", "not_planned", "duplicate"] {
            let parsed = ClosedReason::parse(raw).expect("vocabulary round-trips");
            assert_eq!(parsed.as_str(), raw);
            assert_eq!(
                serde_json::to_string(&parsed).unwrap(),
                format!("\"{raw}\"")
            );
        }
    }

    #[test]
    fn create_request_skips_empty_optionals() {
        let req = TaskCreateRequest {
            title: "ship".into(),
            require: vec!["dev".into()],
            ..Default::default()
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["title"], "ship");
        assert_eq!(json["require"], serde_json::json!(["dev"]));
        assert!(json.get("assignee").is_none());
        assert!(json.get("creator").is_none());
        assert!(json.get("room_id").is_none());
        let back: TaskCreateRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back.title, "ship");
        assert_eq!(back.require, vec!["dev".to_string()]);
        assert!(back.assignee.is_none());
    }

    #[test]
    fn force_flags_default_and_skip() {
        let force = TaskForceRequest { force: false };
        assert!(serde_json::to_value(&force).unwrap().get("force").is_none());
        let forced = TaskForceRequest { force: true };
        assert_eq!(serde_json::to_value(&forced).unwrap()["force"], true);
    }

    #[test]
    fn list_query_parses_lowercase_status() {
        let query: TaskListQuery = serde_json::from_str(r#"{"status":"queued"}"#).unwrap();
        assert_eq!(query.status, Some(TaskStatus::Queued));
        let query: TaskListQuery =
            serde_json::from_str(r#"{"status":"in_progress","archived":true}"#).unwrap();
        assert_eq!(query.status, Some(TaskStatus::InProgress));
        assert!(query.archived);
        assert!(serde_json::from_str::<TaskListQuery>(r#"{"status":"Queued"}"#).is_err());
    }

    #[test]
    fn close_request_carries_the_reason_enum() {
        let json = r#"{"reason":"not_planned","force":true}"#;
        let req: TaskCloseRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.reason, ClosedReason::NotPlanned);
        assert!(req.force);
        assert!(req.summary.is_none());
        assert_eq!(serde_json::to_value(&req).unwrap()["reason"], "not_planned");
    }

    #[test]
    fn close_request_rejects_an_unknown_reason() {
        // The reason vocabulary is closed: an unknown or mis-cased
        // spelling must fail the decode, not coerce into a default.
        assert!(serde_json::from_str::<TaskCloseRequest>(r#"{"reason":"done"}"#).is_err());
        assert!(serde_json::from_str::<TaskCloseRequest>(r#"{"reason":"Completed"}"#).is_err());
    }
}
