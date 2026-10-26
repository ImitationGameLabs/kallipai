//! The turn-history routes: who may read, what is absent, and which
//! inputs belong to the caller.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use just_llm_client::types::generation::Message;
use kallipai_adk::history::{HistoryRecord, RecordKind};
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
        .uri(format!("/agents/{id}/turns"))
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
