//! Task-domain wire types: the request bodies, the export shape, and the
//! state vocabulary shared by the task store, the tagma task routes, and
//! the CLI's task client.
//!
//! One definition per shape: the store's verb signatures, the HTTP
//! bodies, and the client methods all speak these types directly, so
//! all three faces bind to one definition and cannot drift apart.

use serde::{Deserialize, Serialize};

/// The four coarse states. Serialized lowercase snake_case on the wire,
/// matching [`TaskStatus::as_str`] and the SQLite spelling — the CLI's
/// `--status` vocabulary and the wire vocabulary are the same strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    InProgress,
    Review,
    Closed,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Review => "review",
            TaskStatus::Closed => "closed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "queued" => Some(TaskStatus::Queued),
            "in_progress" => Some(TaskStatus::InProgress),
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

/// Body of `POST /tasks`. Every field except the title and creator is
/// optional: a minimal registration is a titled task with a creator.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCreateRequest {
    pub title: String,
    pub creator: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// The seat roster. Absent and empty are the same thing for a
    /// registration: no seats are recorded until dispatch names them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seats: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dossier_path: Option<String>,
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

/// Body of `POST /tasks/{id}/checkpoint`. A checkpoint is a work-log
/// action that may also file a review receipt, move the machine to
/// `review`, or toggle the `waiting` timing marker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCheckpointRequest {
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub receipt: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub review: bool,
    /// Some(true) sets the marker, Some(false) clears it, None leaves
    /// it untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<bool>,
}

/// Body of the force-carrying verbs: `start`, `reopen`, and `archive`.
/// The flag is the auditable escape from the verb's gate; it is
/// recorded in the task's event trail.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskForceRequest {
    pub actor: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

/// Body of the note-carrying verbs: `annotate` and `gate-report`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNoteRequest {
    pub actor: String,
    pub note: String,
}

/// Body of `POST /tasks/{id}/dispatch`. The seats stay an `Option` on
/// purpose: `None` falls back to the roster registered at create,
/// while `Some` names an explicit roster for the close gate to count
/// receipts against — an empty list is an explicit zero-seat dispatch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskDispatchRequest {
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seats: Option<Vec<String>>,
}

/// Body of `POST /tasks/{id}/chain-op`: a recorded chain operation
/// (commit, amend, rebase, reset) that pairs with a preceding
/// gate-report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskChainOpRequest {
    pub actor: String,
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

/// Body of `POST /tasks/{id}/close`. Closing requires a reason; the
/// receipt gate counts dispatch seats against filed receipts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCloseRequest {
    pub actor: String,
    pub reason: ClosedReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
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
#[derive(Serialize, Deserialize)]
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
    pub seats: Vec<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub waiting: bool,
    pub waiting_since: Option<String>,
    /// The archive partition marker (a query partition, not a state).
    pub archived: bool,
    pub archived_at: Option<String>,
    pub closed_reason: Option<String>,
    pub close_summary: Option<String>,
    pub association: Option<AssociationExport>,
    /// Two-phase pointer: live path while open; after close, the content
    /// address (`archive_hash`) is the frozen truth. Both are exported.
    pub dossier_path: Option<String>,
    pub archive_hash: Option<String>,
    pub events: Vec<EventExport>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_wire_spelling_matches_as_str() {
        for raw in ["queued", "in_progress", "review", "closed"] {
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
            creator: "root".into(),
            seats: vec!["dev".into()],
            ..Default::default()
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["title"], "ship");
        assert_eq!(json["seats"], serde_json::json!(["dev"]));
        assert!(json.get("assignee").is_none());
        assert!(json.get("dossier_path").is_none());
        assert!(json.get("room_id").is_none());
        let back: TaskCreateRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back.title, "ship");
        assert_eq!(back.seats, vec!["dev".to_string()]);
        assert!(back.assignee.is_none());
    }

    #[test]
    fn dispatch_keeps_explicit_zero_seats() {
        // None serializes away (the store then falls back to the roster
        // registered at create); Some(vec![]) stays on the wire as an
        // empty array — the explicit zero-seat dispatch.
        let none = TaskDispatchRequest {
            actor: "root".into(),
            seats: None,
        };
        assert!(serde_json::to_value(&none).unwrap().get("seats").is_none());
        let zero = TaskDispatchRequest {
            actor: "root".into(),
            seats: Some(vec![]),
        };
        assert_eq!(
            serde_json::to_value(&zero).unwrap()["seats"],
            serde_json::json!([])
        );
    }

    #[test]
    fn force_and_checkpoint_flags_default_and_skip() {
        let force = TaskForceRequest {
            actor: "root".into(),
            force: false,
        };
        assert!(serde_json::to_value(&force).unwrap().get("force").is_none());
        let forced = TaskForceRequest {
            actor: "root".into(),
            force: true,
        };
        assert_eq!(serde_json::to_value(&forced).unwrap()["force"], true);

        let checkpoint = TaskCheckpointRequest {
            actor: "dev".into(),
            receipt: true,
            ..Default::default()
        };
        let json = serde_json::to_value(&checkpoint).unwrap();
        assert_eq!(json["receipt"], true);
        assert!(json.get("review").is_none());
        assert!(json.get("waiting").is_none());
        assert!(json.get("note").is_none());
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
        let json = r#"{"actor":"dev","reason":"not_planned","force":true}"#;
        let req: TaskCloseRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.reason, ClosedReason::NotPlanned);
        assert!(req.force);
        assert!(req.summary.is_none());
        assert_eq!(serde_json::to_value(&req).unwrap()["reason"], "not_planned");
    }

    #[test]
    fn waiting_marker_wire_carries_all_three_states() {
        // Absent decodes to None (leave untouched) and skips on the way
        // back out; an explicit boolean survives verbatim both ways, so
        // set and clear are distinguishable on the wire.
        let untouched: TaskCheckpointRequest =
            serde_json::from_str(r#"{"actor":"dev","note":"n"}"#).unwrap();
        assert_eq!(untouched.waiting, None);
        assert!(
            serde_json::to_value(&untouched)
                .unwrap()
                .get("waiting")
                .is_none()
        );

        let set: TaskCheckpointRequest =
            serde_json::from_str(r#"{"actor":"dev","note":"n","waiting":true}"#).unwrap();
        assert_eq!(set.waiting, Some(true));
        assert_eq!(
            serde_json::to_value(&set).unwrap()["waiting"],
            serde_json::json!(true)
        );

        let clear: TaskCheckpointRequest =
            serde_json::from_str(r#"{"actor":"dev","note":"n","waiting":false}"#).unwrap();
        assert_eq!(clear.waiting, Some(false));
        assert_eq!(
            serde_json::to_value(&clear).unwrap()["waiting"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn chain_op_request_wire_round_trips() {
        // The full shape and the minimal one: `detail` skips when absent
        // and `force` defaults to false on decode — the auditable escape
        // is opt-in on the wire.
        let full: TaskChainOpRequest = serde_json::from_str(
            r#"{"actor":"root","op":"rebase","detail":"five commits","force":true}"#,
        )
        .unwrap();
        assert_eq!(full.op, "rebase");
        assert_eq!(full.detail.as_deref(), Some("five commits"));
        assert!(full.force);

        let minimal: TaskChainOpRequest =
            serde_json::from_str(r#"{"actor":"root","op":"commit"}"#).unwrap();
        assert!(minimal.detail.is_none());
        assert!(!minimal.force);
        let wire = serde_json::to_value(&minimal).unwrap();
        assert!(wire.get("detail").is_none());
        assert!(wire.get("force").is_none());
    }

    #[test]
    fn close_request_rejects_an_unknown_reason() {
        // The reason vocabulary is closed: an unknown or mis-cased
        // spelling must fail the decode, not coerce into a default.
        assert!(
            serde_json::from_str::<TaskCloseRequest>(r#"{"actor":"dev","reason":"done"}"#).is_err()
        );
        assert!(
            serde_json::from_str::<TaskCloseRequest>(r#"{"actor":"dev","reason":"Completed"}"#)
                .is_err()
        );
    }
}
