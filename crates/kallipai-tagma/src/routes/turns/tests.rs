//! The turn-history routes: who may read, what is absent, and which
//! inputs belong to the caller.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use just_llm_client::types::generation::{Message, ToolCall};
use kallipai_adk::history::{HistoryRecord, RecordKind};
use kallipai_common::protocol::CALL_BODY_MAX_BYTES;
use tempfile::TempDir;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;

use super::*;
use crate::auth::{AuthIdentity, Identity};
use crate::state::{RegistryEntry, SharedState};
use crate::test_helpers::{make_entry, make_faulted_entry, make_state};

/// Write one day file of turn records, oldest first.
fn write_turns(dir: &FsPath, records: Vec<HistoryRecord>) {
    let history = dir.join("history");
    std::fs::create_dir_all(&history).unwrap();
    let mut text = String::new();
    for record in records {
        text.push_str(&serde_json::to_string(&record).unwrap());
        text.push('\n');
    }
    std::fs::write(history.join("2026-10-01.ndjson"), text).unwrap();
}

fn turn(turn_id: u64, minute: u8, messages: Vec<Message>) -> HistoryRecord {
    let at = format!("2026-10-01T10:{minute:02}:00Z");
    HistoryRecord {
        datetime: OffsetDateTime::parse(&at, &Rfc3339).unwrap(),
        turn_id: Some(turn_id),
        messages,
        estimated_tokens: 1,
        kind: RecordKind::Turn,
        event: None,
        attachments: Vec::new(),
        attempt: None,
    }
}

/// A live agent whose history log holds two turns.
async fn agent_with_turns(state: &SharedState) -> (AgentId, TempDir) {
    let id = AgentId::random();
    let dir = TempDir::new().unwrap();
    write_turns(
        dir.path(),
        vec![
            turn(1, 0, vec![Message::user("first")]),
            turn(2, 1, vec![Message::user("second")]),
        ],
    );
    let mut entry = make_entry(None, format!("agent-{id}"));
    entry.identity.agent_dir = Some(dir.path().to_path_buf());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    (id, dir)
}

/// Any authenticated identity reads any agent's turns: the log is this
/// tagma's audit surface, not a per-agent private file. All three reads work
/// off the same directory.
#[tokio::test]
async fn any_agent_reads_another_agents_turns() {
    let state = make_state();
    let (id, _dir) = agent_with_turns(&state).await;
    let auth = AuthIdentity::test_new(Identity::Agent {
        id: AgentId::random(),
    });

    let page = list_turns(
        State(state.clone()),
        auth.clone(),
        Path(id.clone()),
        Query(TurnsQuery::default()),
    )
    .await
    .expect("a peer agent may read");
    let ids: Vec<u64> = page.0.rows.iter().map(|row| row.turn_id).collect();
    assert_eq!(ids, vec![2, 1], "newest first");

    let detail = get_turn(State(state.clone()), auth.clone(), Path((id.clone(), 1)))
        .await
        .expect("the turn reads");
    assert_eq!(detail.0.turn.messages[0].text.as_deref(), Some("first"));

    let body = get_call_body(
        State(state),
        auth,
        Path((id, 1, 0)),
        Query(CallBodyQuery::default()),
    )
    .await;
    assert_eq!(
        body.unwrap_err().status,
        404,
        "a turn with no tool calls has no call 0"
    );
}

/// The operator reads the same surface.
#[tokio::test]
async fn operator_reads_any_agent_turns() {
    let state = make_state();
    let (id, _dir) = agent_with_turns(&state).await;
    let page = list_turns(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(id),
        Query(TurnsQuery::default()),
    )
    .await
    .expect("the operator may read");
    assert_eq!(page.0.rows.len(), 2);
}

/// An agent the registry does not know is a 404, not an empty page.
#[tokio::test]
async fn an_unknown_agent_is_404() {
    let state = make_state();
    let error = list_turns(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(AgentId::random()),
        Query(TurnsQuery::default()),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, 404);
}

/// A faulted agent keeps its directory, so its turns stay readable — the case
/// the audit face exists for.
#[tokio::test]
async fn a_faulted_agent_still_reads() {
    let state = make_state();
    let dir = TempDir::new().unwrap();
    write_turns(
        dir.path(),
        vec![turn(7, 0, vec![Message::user("last words")])],
    );
    let id = AgentId::random();
    let mut entry = make_faulted_entry(None, "restore failed: boom");
    entry.identity.agent_dir = Some(dir.path().to_path_buf());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Faulted(entry));

    let page = list_turns(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(id),
        Query(TurnsQuery::default()),
    )
    .await
    .expect("a dead agent stays auditable");
    assert_eq!(page.0.rows.len(), 1);
}

/// An agent that never wrote a turn is a 404: an empty page would read as an
/// agent whose log is empty.
#[tokio::test]
async fn an_agent_without_a_log_is_404() {
    let state = make_state();
    let dir = TempDir::new().unwrap();
    let id = AgentId::random();
    let mut entry = make_entry(None, format!("agent-{id}"));
    entry.identity.agent_dir = Some(dir.path().to_path_buf());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));

    let error = list_turns(
        State(state),
        AuthIdentity::test_new(Identity::Operator),
        Path(id),
        Query(TurnsQuery::default()),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, 404);
}

/// The `error.message` a refusal puts on the wire.
fn reason(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value["error"]["message"].as_str().map(str::to_owned)
}

/// Malformed inputs belong to the caller: a bad day or a bad cursor is a
/// 400 from the route, not an absent log and not an empty page.
#[tokio::test]
async fn a_bad_day_or_cursor_is_400() {
    let state = make_state();
    let (id, _dir) = agent_with_turns(&state).await;
    let auth = AuthIdentity::test_new(Identity::Operator);

    let bad_day = list_turns(
        State(state.clone()),
        auth.clone(),
        Path(id.clone()),
        Query(TurnsQuery {
            day: Some("2026-10-1".to_owned()),
            before: None,
            limit: None,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(bad_day.status, 400);
    let echoed = axum::body::to_bytes(bad_day.into_response().into_body(), 1 << 16)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&echoed);
    assert!(
        !text.contains("2026-10-1"),
        "the caller's own text is not echoed back: {text}"
    );
    let day_reason = reason(&echoed).expect("a 400 carries a reason the caller can read");
    assert!(!day_reason.is_empty(), "the reason is not empty: {text}");

    let bad_cursor = list_turns(
        State(state),
        auth,
        Path(id),
        Query(TurnsQuery {
            day: None,
            before: Some("nonsense".to_owned()),
            limit: None,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(bad_cursor.status, 400);
    let echoed = axum::body::to_bytes(bad_cursor.into_response().into_body(), 1 << 16)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&echoed);
    let cursor_reason = reason(&echoed).expect("a 400 carries a reason the caller can read");
    assert!(!cursor_reason.is_empty(), "the reason is not empty: {text}");
    assert!(
        !text.contains("nonsense"),
        "the caller's own text is not echoed back: {text}"
    );
    assert_ne!(
        day_reason, cursor_reason,
        "each refusal names its own fault: {day_reason} / {cursor_reason}"
    );
}

/// A bearer the tagma never issued is refused by the auth layer, before any
/// handler sees the request.
#[tokio::test]
async fn a_foreign_bearer_is_401() {
    let state = make_state();
    let (id, _dir) = agent_with_turns(&state).await;
    let request = Request::builder()
        .uri(format!("/agents/{id}/history"))
        .header(header::AUTHORIZATION, "Bearer not-a-token")
        .body(Body::empty())
        .unwrap();
    let response = crate::routes::router()
        .with_state(state)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// A message body reads the text the detail face shortened, and an index the
/// turn does not have is 404 rather than an empty range.
#[tokio::test]
async fn a_message_body_reads_the_whole_text_and_refuses_a_missing_index() {
    let state = make_state();
    let (id, _dir) = agent_with_turns(&state).await;
    let auth = AuthIdentity::test_new(Identity::Agent {
        id: AgentId::random(),
    });

    let body = get_message_body(
        State(state.clone()),
        auth.clone(),
        Path((id.clone(), 1, 0)),
        Query(MessageBodyQuery::default()),
    )
    .await
    .expect("the first message reads");
    assert_eq!(body.0.turn_id, 1);
    assert_eq!(body.0.role, "user");
    assert_eq!(body.0.text, "first");
    assert_eq!(body.0.part, kallipai_common::protocol::TextPart::Text);

    let missing = get_message_body(
        State(state),
        auth,
        Path((id, 1, 9)),
        Query(MessageBodyQuery::default()),
    )
    .await
    .unwrap_err();
    assert_eq!(missing.status, 404);
}

/// A live agent whose one call answered with a stream past the body cap.
async fn agent_with_a_long_call(state: &SharedState) -> (AgentId, TempDir) {
    let id = AgentId::random();
    let dir = TempDir::new().unwrap();
    let output = "x".repeat(CALL_BODY_MAX_BYTES + 4_096);
    let envelope = serde_json::json!({
        "ok": true,
        "tool_name": "bash_exec",
        "result": {"output": output, "exit_code": 0, "timed_out": false, "truncated": false},
    });
    let call = ToolCall {
        id: "c1".to_owned(),
        name: "bash_exec".to_owned(),
        arguments: "{}".to_owned(),
    };
    write_turns(
        dir.path(),
        vec![turn(
            1,
            0,
            vec![
                Message::assistant_tool_calls(None, vec![call], None),
                Message::tool(serde_json::to_string(&envelope).unwrap(), "c1"),
            ],
        )],
    );
    let mut entry = make_entry(None, format!("agent-{id}"));
    entry.identity.agent_dir = Some(dir.path().to_path_buf());
    state
        .registry
        .write()
        .await
        .register(id.clone(), RegistryEntry::Live(entry));
    (id, dir)
}

/// The two body reads take their own part type, so a value from the other
/// family is refused before any log is read: `args` is a call's part, and a
/// message body answering it would have to guess.
#[tokio::test]
async fn a_part_from_the_other_family_is_400() {
    let state = make_state();
    let (id, _dir) = agent_with_turns(&state).await;

    for uri in [
        format!("/agents/{id}/history/1/messages/0/body?part=args"),
        format!("/agents/{id}/history/1/calls/0/body?part=reasoning"),
    ] {
        let request = Request::builder()
            .uri(&uri)
            .header(header::AUTHORIZATION, "Bearer op-token")
            .body(Body::empty())
            .unwrap();
        let response = crate::routes::router()
            .with_state(state.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{uri} names a part the other reader owns"
        );
    }
}

/// `limit` is the caller's wish, clamped into the bounds the face promises:
/// asking for nothing still returns a byte, and asking for everything stops
/// at the cap with a next offset to continue from.
#[tokio::test]
async fn a_body_limit_is_clamped_into_the_bounds() {
    let state = make_state();
    let (id, _dir) = agent_with_a_long_call(&state).await;
    let auth = AuthIdentity::test_new(Identity::Operator);

    let least = get_call_body(
        State(state.clone()),
        auth.clone(),
        Path((id.clone(), 1, 0)),
        Query(CallBodyQuery {
            limit: Some(0),
            ..CallBodyQuery::default()
        }),
    )
    .await
    .expect("a zero limit is clamped, not refused");
    assert_eq!(least.0.text.chars().count(), 1, "one byte is the floor");
    assert!(least.0.truncated);
    assert_eq!(least.0.next_offset, Some(1));

    let most = get_call_body(
        State(state),
        auth,
        Path((id, 1, 0)),
        Query(CallBodyQuery {
            limit: Some(usize::MAX),
            ..CallBodyQuery::default()
        }),
    )
    .await
    .expect("a huge limit is clamped, not refused");
    assert_eq!(
        most.0.text.len(),
        CALL_BODY_MAX_BYTES,
        "the cap bounds one response"
    );
    assert_eq!(most.0.next_offset, Some(CALL_BODY_MAX_BYTES as u64));
}

/// A part the record does not hold is 404, and the reason names the part, so
/// a reader can tell "no reasoning text" from "no message".
#[tokio::test]
async fn a_missing_part_is_404_and_names_the_part() {
    let state = make_state();
    let (id, _dir) = agent_with_turns(&state).await;
    let auth = AuthIdentity::test_new(Identity::Operator);

    let error = get_message_body(
        State(state),
        auth,
        Path((id, 1, 0)),
        Query(MessageBodyQuery {
            part: Some(kallipai_common::protocol::TextPart::Reasoning),
            ..MessageBodyQuery::default()
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, 404);
    let rendered = axum::body::to_bytes(error.into_response().into_body(), 1 << 16)
        .await
        .unwrap();
    let text = reason(&rendered).expect("a 404 carries a reason the caller can read");
    assert!(
        text.contains("reasoning"),
        "the refusal names the part: {text}"
    );
}
