//! In-process management-op dispatch: route a TagmaControl::Manage through
//! the management-plane subset of the tagma router and emit the ManageResult.
//!
//! The subset reuses the same axum Router DSL as [`crate::routes::router`],
//! so route syntax cannot fork between the two tables; the management plane
//! deliberately exposes only the operator surface (no per-agent inbox,
//! dirlock, approval, or agent-creation routes over the relay). The relay IS
//! the trusted operator-equivalent (same as dispatch::execute_op's
//! `Identity::Operator`); it injects that identity via request extensions,
//! and no HTTP loopback, token, or SSRF is involved. The single caller is the
//! manage dispatch arm in bilateral::handle_user_op; handle_manage is
//! pub(super).
//! The subset includes the root agent's lesche voice (the session list,
//! direct-session history, and send): owner-console conversation data
//! served under the same trust model (the relay is the operator-equivalent).

use super::RelayHandle;
use crate::auth::AuthIdentity;
use crate::routes::{agent, budget, context, lesche, message, profile_probe, profiles};
use crate::state::SharedState;
use crate::work_schedule;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt;
use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::ListAgentsQuery;
use kallipai_lesche_common::message::TagmaReply;
use std::panic::AssertUnwindSafe;
use std::str::FromStr;
use tower::ServiceExt;
use tracing::{error, warn};
/// The relay's own response cap, applied where the response body is taken:
/// it bounds the plaintext manage frame and the envelope path alike. A reply
/// over this many bytes is not carried, and the caller gets a `502` whose
/// message reads `response body too large`.
pub(crate) const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// The management-plane route table: the same handlers and DSL as
/// [`crate::routes::router`], restricted to the deliberate operator subset.
fn manage_router() -> Router<SharedState> {
    use axum::routing::{delete, get, post, put};
    Router::new()
        .route(
            "/budget",
            get(budget::get_budget).post(budget::update_budget),
        )
        .route("/agents", get(list_agents_lenient))
        .route("/agents/{id}", delete(agent::remove_agent))
        .route("/agents/{id}/status", get(context::agent_status))
        .route("/agents/{id}/interrupt", post(agent::interrupt_agent))
        .route("/agents/{id}/duty", put(agent::update_duty))
        .route("/agents/{id}/turns", get(crate::routes::turns::list_turns))
        .route(
            "/agents/{id}/turns/{turn_id}",
            get(crate::routes::turns::get_turn),
        )
        .route(
            "/agents/{id}/turns/{turn_id}/calls/{call_index}/body",
            get(crate::routes::turns::get_call_body),
        )
        .route("/agents/{id}/metadata", put(agent::update_metadata))
        .route("/agents/{id}/profile-set", put(agent::update_profile_set))
        .route(
            "/profiles",
            get(profiles::get_profiles).put(profiles::put_profiles),
        )
        .route("/profiles/probe", post(profile_probe::probe_profiles))
        .route("/profiles/apply", post(profiles::apply_profiles))
        .route("/profiles/refresh", post(profiles::refresh_profile_source))
        .route("/profiles/default", put(profiles::set_default_profile_set))
        .route(
            "/profiles/sets/{name}",
            delete(profiles::delete_profile_set),
        )
        .route(
            "/work-schedule",
            get(work_schedule::get_work_schedule).put(work_schedule::put_work_schedule),
        )
        .route(
            "/agents/{id}/lesche/sessions",
            get(lesche::list_lesche_sessions),
        )
        .route(
            "/agents/{id}/lesche/direct-sessions/{peer}/messages",
            get(lesche::read_direct_session_messages),
        )
        .route("/agents/{id}/lesche/messages", post(lesche::post_message))
        // A read cursor is partition-scoped, and this router learns the
        // partition from the `ReadPeer` extension, which only the relay
        // envelope path inserts. A plaintext manage frame carries no peer
        // identity (see `handle_manage_rest`), so a PUT arriving that way
        // would advance the OPERATOR partition -- the wrong conversation.
        // The frame allowlist in `relay::tunnel` therefore denies this path
        // (pinned by `manage_rest_tests`), so the E2E envelope is the only
        // caller that can carry a peer into this route.
        .route(
            "/agents/{id}/external/read-cursor",
            put(message::external_read_cursor),
        )
        .fallback(manage_fallback)
}

/// One-registry-pass aggregate (lock only for summaries): every agent's
/// summary, optionally joined with its live status snapshot -- the N+1
/// killer for the operator UI: one frame instead of one status GET per
/// agent.
async fn aggregate_agents(state: &SharedState, with_status: bool) -> Response {
    // Phase 1 -- hold the registry read lock only while snapshotting
    // summaries, then DROP it before the per-agent status joins: agent_status
    // takes the same read lock, and holding ours across those awaits would
    // let an interleaving writer queue ahead of the readers and deadlock
    // the node (tokio RwLock is write-preferring).
    let mut agents = {
        let registry = state.registry.read().await;
        let mut agents = Vec::new();
        for (id, entry) in registry.iter() {
            let mut summary =
                serde_json::to_value(state.summarize(id, entry)).unwrap_or(serde_json::Value::Null);
            if with_status {
                summary["status"] = serde_json::Value::Null; // joined below
            }
            agents.push((entry.as_live().is_some(), summary));
        }
        agents
    };
    if with_status {
        for (live, summary) in agents.iter_mut() {
            if !*live {
                continue;
            }
            let id = AgentId::from(summary["id"].as_str().unwrap_or("").to_owned());
            // Reuse the real handler in-process: one function call per
            // agent, not one HTTP round-trip per agent.
            let resp = crate::routes::context::agent_status(
                State(state.clone()),
                crate::auth::AuthIdentity::operator(),
                axum::extract::Path(id),
            )
            .await
            .into_response();
            let bytes = if resp.status().is_success() {
                axum::body::to_bytes(resp.into_body(), 1 << 20)
                    .await
                    .unwrap_or_default()
            } else {
                axum::body::Bytes::new()
            };
            summary["status"] = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        }
    }
    let agents: Vec<serde_json::Value> = agents.into_iter().map(|(_, s)| s).collect();
    axum::Json(serde_json::json!({ "agents": agents })).into_response()
}

/// `list_agents` with the manage plane's historical query semantics: a
/// missing or unparsable query string yields the default struct (axum's
/// `Query` extractor would reject with 400 instead).
async fn list_agents_lenient(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    uri: Uri,
    body: axum::body::Bytes,
) -> Response {
    // Aggregated shape: the reverse proxy turns the HTTP-side
    // `?include=` filter into this frame-body field -- the frame path
    // itself stays query-free. Include keys are a closed allowlist;
    // unknown keys are rejected (400) rather than ignored, matching the
    // frame surface's fail-closed stance.
    let parsed: serde_json::Value =
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    if let Some(inc) = parsed.get("include") {
        let includes: Vec<String> = match serde_json::from_value(inc.clone()) {
            Ok(v) => v,
            Err(_) => {
                return axum::http::StatusCode::BAD_REQUEST.into_response();
            }
        };
        const ALLOWED: [&str; 1] = ["status"];
        if includes.iter().any(|k| !ALLOWED.contains(&k.as_str())) {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        return aggregate_agents(&state, includes.contains(&"status".to_string())).await;
    }
    let q: ListAgentsQuery =
        serde_urlencoded::from_str(uri.query().unwrap_or("")).unwrap_or_default();
    agent::list_agents(State(state), auth, axum::extract::Query(q))
        .await
        .into_response()
}

/// Keep the old string-router catch-all 404 body verbatim.
async fn manage_fallback() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({"error":{"message":"unknown management route"}})),
    )
        .into_response()
}

/// The same 404 shape, as a reply for malformed methods/URIs that cannot
/// reach the router.
fn not_found_reply() -> TagmaReply {
    TagmaReply::ManageResult {
        req_id: 0,
        status: 404,
        body: serde_json::json!({"error":{"message":"unknown management route"}}),
    }
}

impl RelayHandle {
    pub(super) async fn handle_manage(
        &self,
        trace: &kallipai_archeion_common::ids::TraceId,
        req_id: u64,
        method: &str,
        path: &str,
        body: serde_json::Value,
        peer: Option<&str>,
    ) {
        let result = AssertUnwindSafe(self.dispatch_manage(method, path, body, peer))
            .catch_unwind()
            .await;
        let reply = match result {
            Ok(resp) => stamp_req_id(resp, req_id),
            Err(_) => {
                error!(req_id, "manage dispatch panicked; emitting 502");
                TagmaReply::ManageResult {
                    req_id,
                    status: 502,
                    body: serde_json::json!({"error":{"message":"relay manage panicked"}}),
                }
            }
        };
        if let Err(e) = self.emit(trace, self.agent_sender(), reply, None).await {
            warn!(req_id, "manage emit: {e:#}");
        }
    }

    pub(super) async fn dispatch_manage(
        &self,
        method: &str,
        path: &str,
        body: serde_json::Value,
        peer: Option<&str>,
    ) -> TagmaReply {
        let Some(state) = self.inner.state.upgrade() else {
            return TagmaReply::ManageResult {
                req_id: 0,
                status: 503,
                body: serde_json::json!({"error":{"message":"tagma shutting down"}}),
            };
        };
        // Reuse the real router: the management-plane table above is the same
        // axum DSL as routes::router, so the two cannot drift into different
        // syntaxes. Malformed methods/URIs land in the same 404 shape the old
        // string-match catch-all produced.
        let Ok(method) = Method::from_str(&method.to_ascii_uppercase()) else {
            return not_found_reply();
        };
        let Ok(uri) = Uri::from_str(path) else {
            return not_found_reply();
        };
        let mut request = match Request::builder()
            .method(method)
            .uri(uri)
            // The handlers' Json extractors require a JSON content type; the
            // relay always carries a JSON body.
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
        {
            Ok(request) => request,
            // Unreachable: method and URI parsed successfully above.
            Err(_) => return not_found_reply(),
        };
        // The relay is the trusted operator for management ops (the same
        // trust execute_op grants deliver_message); say so through the
        // standard extractor instead of hand-passing the argument per arm.
        request.extensions_mut().insert(AuthIdentity::operator());
        // The conversation partition the request targets, when the caller arrived
        // on an authenticated relay envelope. The plaintext manage-frame surface
        // carries no peer identity, so its requests stay on the operator partition.
        if let Some(peer) = peer {
            request
                .extensions_mut()
                .insert(message::ReadPeer(peer.to_string()));
        }
        let response = match manage_router().with_state(state).oneshot(request).await {
            Ok(response) => response,
            Err(infallible) => match infallible {},
        };
        extract_response(response).await
    }
}

fn stamp_req_id(mut reply: TagmaReply, req_id: u64) -> TagmaReply {
    if let TagmaReply::ManageResult { req_id: r, .. } = &mut reply {
        *r = req_id;
    }
    reply
}

async fn extract_response(response: axum::response::Response) -> TagmaReply {
    let (parts, body_bytes) = response.into_parts();
    let status = parts.status.as_u16();
    let bytes = match to_bytes(body_bytes, MAX_RESPONSE_BYTES).await {
        Ok(b) => b,
        Err(e) => {
            warn!("manage response body extraction failed: {e}");
            return TagmaReply::ManageResult {
                req_id: 0,
                status: 502,
                body: serde_json::json!({"error":{"message":"response body too large"}}),
            };
        }
    };
    let body_value: serde_json::Value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    TagmaReply::ManageResult {
        req_id: 0,
        status,
        body: body_value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::UnreadFrame;
    use crate::relay::chat_history;
    use crate::state::RegistryEntry;
    use crate::test_helpers::{
        alt_bound_sub, install_projector, make_entry_with_rx, make_state, make_state_two_sets,
    };
    use just_llm_client::types::generation::{Message, ToolCall};
    use kallipai_adk::history::{HistoryRecord, RecordKind};
    use kallipai_common::toolresult::ToolResultEnvelope;
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    async fn manage_get(state: &SharedState, uri: &str) -> Response {
        let request = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .extension(AuthIdentity::operator())
            .body(Body::empty())
            .expect("static parts");
        manage_router()
            .with_state(state.clone())
            .oneshot(request)
            .await
            .unwrap_or_else(|infallible| match infallible {})
    }
    /// Drive the management router the way the relay envelope does: operator
    /// auth, a JSON content type, an optional body, and -- for a request that
    /// arrived on an authenticated envelope -- the conversation peer the
    /// router hands the handler. The full-table shape nail needs each route's
    /// declared verb, not just GET.
    async fn manage_send(
        state: &SharedState,
        method: &str,
        uri: &str,
        peer: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Response {
        let method = Method::from_str(method).expect("static method");
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .extension(AuthIdentity::operator());
        if let Some(peer) = peer {
            builder = builder.extension(message::ReadPeer(peer.to_string()));
        }
        let request = builder
            .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
            .expect("static parts");
        manage_router()
            .with_state(state.clone())
            .oneshot(request)
            .await
            .unwrap_or_else(|infallible| match infallible {})
    }

    /// [`manage_send`] for a request that carries neither a body nor a peer.
    async fn manage_call(state: &SharedState, method: &str, uri: &str) -> Response {
        manage_send(state, method, uri, None, None).await
    }

    /// Route-shape nail: every (method, path) row of the management-plane
    /// table outside the three turn routes must survive the router's
    /// extractors. A Path-shape mismatch (route capture count vs extractor
    /// arity) is a 500 by axum contract. The turn routes are left out here: a
    /// 404 cannot tell a registered route from an absent one, and the
    /// positive test below covers their registration and shape.
    /// 400/404/405 are legitimate handler outcomes here;
    /// only the 500 shape-rejection fires the assert.
    #[tokio::test]
    async fn manage_router_shape_pin_rejects_no_path_shape_500s() {
        let state = make_state();
        let rows: &[(&str, &str)] = &[
            ("GET", "/budget"),
            ("POST", "/budget"),
            ("GET", "/agents"),
            ("DELETE", "/agents/a-1"),
            ("GET", "/agents/a-1/status"),
            ("POST", "/agents/a-1/interrupt"),
            ("PUT", "/agents/a-1/duty"),
            ("PUT", "/agents/a-1/metadata"),
            ("PUT", "/agents/a-1/profile-set"),
            ("GET", "/profiles"),
            ("PUT", "/profiles"),
            ("POST", "/profiles/probe"),
            ("POST", "/profiles/apply"),
            ("POST", "/profiles/refresh"),
            ("PUT", "/profiles/default"),
            ("DELETE", "/profiles/sets/default"),
            ("GET", "/work-schedule"),
            ("PUT", "/work-schedule"),
            ("GET", "/agents/a-1/lesche/sessions"),
            ("GET", "/agents/a-1/lesche/direct-sessions/peer-9/messages"),
            ("POST", "/agents/a-1/lesche/messages"),
            ("PUT", "/agents/a-1/external/read-cursor"),
        ];
        for (method, path) in rows {
            // The peer rides along: the read-cursor route's `ReadPeer`
            // extension is required, and a request without one is a
            // missing-extension 500 that says nothing about the path shape.
            let resp = manage_send(&state, method, path, Some("peer-1"), None).await;
            assert_ne!(
                resp.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "path-shape rejection on {method} {path}"
            );
        }
    }

    /// The turns read face is the one production path these routes have: a
    /// client holding a session text reaches them only through the
    /// management router, so the router itself has to answer with rows.
    #[tokio::test]
    async fn the_turns_routes_answer_through_the_management_router() {
        let state = make_state();
        let dir = tempfile::TempDir::new().unwrap();
        let history = dir.path().join("history");
        std::fs::create_dir_all(&history).unwrap();
        let record = HistoryRecord {
            datetime: OffsetDateTime::parse("2026-10-01T10:00:00Z", &Rfc3339).unwrap(),
            turn_id: Some(4),
            messages: vec![
                Message::user("hi"),
                Message::assistant_tool_calls(
                    None,
                    vec![ToolCall {
                        id: "c1".to_owned(),
                        name: "bash_exec".to_owned(),
                        arguments: "{}".to_owned(),
                    }],
                    None,
                ),
                Message::tool(
                    serde_json::to_string(&ToolResultEnvelope {
                        ok: true,
                        tool_name: "bash_exec".to_owned(),
                        result: Some(serde_json::json!({
                            "output": "the tool said so",
                            "exit_code": 0,
                            "timed_out": false,
                            "truncated": false,
                        })),
                        error: None,
                        pending_approval: None,
                        rest: serde_json::Map::new(),
                    })
                    .unwrap(),
                    "c1",
                ),
            ],
            estimated_tokens: 1,
            kind: RecordKind::Turn,
            event: None,
            attachments: Vec::new(),
            attempt: None,
        };
        std::fs::write(
            history.join("2026-10-01.ndjson"),
            serde_json::to_string(&record).unwrap() + "\n",
        )
        .unwrap();
        let id = AgentId::random();
        let (mut entry, _rx) = make_entry_with_rx(None, format!("agent-{id}"));
        entry.identity.agent_dir = Some(dir.path().to_path_buf());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let page = manage_send(
            &state,
            "GET",
            &format!("/agents/{id}/turns"),
            Some("peer-1"),
            None,
        )
        .await;
        assert_eq!(page.status(), StatusCode::OK, "the page route answers");
        let bytes = axum::body::to_bytes(page.into_body(), MAX_RESPONSE_BYTES)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["rows"][0]["turn_id"], 4, "a row comes back");

        let detail = manage_send(
            &state,
            "GET",
            &format!("/agents/{id}/turns/4"),
            Some("peer-1"),
            None,
        )
        .await;
        assert_eq!(detail.status(), StatusCode::OK, "the detail route answers");

        let body = manage_send(
            &state,
            "GET",
            &format!("/agents/{id}/turns/4/calls/0/body"),
            Some("peer-1"),
            None,
        )
        .await;
        assert_eq!(body.status(), StatusCode::OK, "the body route answers");
        let bytes = axum::body::to_bytes(body.into_body(), MAX_RESPONSE_BYTES)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            body["text"]
                .as_str()
                .unwrap_or_default()
                .contains("the tool said so"),
            "the range carries the tool's own text: {body}"
        );
    }

    /// A page row that alone outgrows the relay's response cap: the page cap
    /// keeps the newest row whatever it weighs, so one turn renders a page over
    /// the cap, and the relay's extraction fails that read instead of carrying
    /// it. The router itself answers 200 -- the failure is the relay's.
    /// An older round in the same file gives the cap loop its drop step: that
    /// row goes first and the newest one stays, still over the cap.
    #[tokio::test]
    async fn a_page_row_past_the_relay_cap_fails_the_read() {
        let state = make_state();
        let dir = tempfile::TempDir::new().unwrap();
        let history = dir.path().join("history");
        std::fs::create_dir_all(&history).unwrap();
        let arguments = "x".repeat(400);
        let calls: Vec<ToolCall> = (0..700u32)
            .map(|call| ToolCall {
                id: format!("c{call}"),
                name: "bash_exec".to_owned(),
                arguments: arguments.clone(),
            })
            .collect();
        let record = HistoryRecord {
            datetime: OffsetDateTime::parse("2026-10-01T10:00:00Z", &Rfc3339).unwrap(),
            turn_id: Some(2),
            messages: vec![Message::assistant_tool_calls(None, calls, None)],
            estimated_tokens: 1,
            kind: RecordKind::Turn,
            event: None,
            attachments: Vec::new(),
            attempt: None,
        };
        let mut older = record.clone();
        older.datetime = OffsetDateTime::parse("2026-10-01T09:00:00Z", &Rfc3339).unwrap();
        older.turn_id = Some(1);
        older.messages = vec![Message::user("earlier prompt")];
        let mut lines = serde_json::to_string(&older).unwrap();
        lines.push('\n');
        lines.push_str(&serde_json::to_string(&record).unwrap());
        lines.push('\n');
        std::fs::write(history.join("2026-10-01.ndjson"), lines).unwrap();
        let id = AgentId::random();
        let (mut entry, _rx) = make_entry_with_rx(None, format!("agent-{id}"));
        entry.identity.agent_dir = Some(dir.path().to_path_buf());
        state
            .registry
            .write()
            .await
            .register(id.clone(), RegistryEntry::Live(entry));

        let page = manage_send(
            &state,
            "GET",
            &format!("/agents/{id}/turns"),
            Some("peer-1"),
            None,
        )
        .await;
        assert_eq!(
            page.status(),
            StatusCode::OK,
            "the route renders the row the relay cannot carry"
        );
        // The cap lives in the relay's extraction of the response, so this is
        // the step that has to fail: the page body is over it.
        let reply = extract_response(page).await;
        let TagmaReply::ManageResult { status, body, .. } = reply else {
            unreachable!("extraction always answers a manage result")
        };
        assert_eq!(status, 502, "the relay cap fails the page read");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "a 502 carries a reason the caller can read: {body}"
        );
    }

    /// The positive seam the shape pin cannot reach: a cursor write through
    /// the real router lands in the partition the envelope's peer names. The
    /// store caps the value at that partition's own newest row id, so an id a
    /// sibling partition minted (one global sequence) cannot pin the badge at
    /// zero, and no other partition moves.
    /// The write also re-announces the partition's snapshot on the bus -- the
    /// badge drop on this user's other devices rides that frame.
    #[tokio::test]
    async fn external_read_cursor_writes_the_peer_partition() {
        let state = make_state();
        let root = AgentId::random();
        let (entry, _rx) = make_entry_with_rx(None, "root".into());
        state
            .registry
            .write()
            .await
            .register_root(root.clone(), RegistryEntry::Live(entry))
            .unwrap();
        let (db, _frames, _dir) = install_projector(&state).await;
        let mut unread_rx = state.bus.subscribe::<UnreadFrame>().unwrap();
        let mut alice = Vec::new();
        for text in ["a1", "a2"] {
            let (id, _) = chat_history::append(
                &db,
                Some("alice"),
                Some("alice"),
                "outbound",
                text,
                None,
                None,
            )
            .await
            .unwrap();
            alice.push(id);
        }
        let (bob, _) =
            chat_history::append(&db, Some("bob"), Some("bob"), "outbound", "b1", None, None)
                .await
                .unwrap();
        assert!(bob > *alice.last().unwrap(), "one global id sequence");

        let response = manage_send(
            &state,
            "PUT",
            &format!("/agents/{root}/external/read-cursor"),
            Some("alice"),
            Some(serde_json::json!({ "cursor": bob })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        assert_eq!(
            chat_history::read_cursor(&db, Some("alice")).await.unwrap(),
            Some(*alice.last().unwrap()),
            "capped at alice's own newest row, not the id bob minted"
        );
        assert_eq!(
            chat_history::read_cursor(&db, Some("bob")).await.unwrap(),
            None,
            "the sibling partition is untouched"
        );
        assert_eq!(
            chat_history::read_cursor(&db, None).await.unwrap(),
            None,
            "the operator partition is untouched"
        );

        let frame = unread_rx
            .try_recv()
            .expect("the write announces a snapshot");
        assert_eq!(frame.0.peer, "alice");
        assert_eq!(frame.0.cursor, *alice.last().unwrap());
        assert_eq!(
            frame.0.unread, 0,
            "the capped cursor reads the partition clean"
        );
    }

    /// The aggregate include list is a closed allowlist -- an
    /// unknown key is a 400 (fail-closed), not an ignored filter.
    #[tokio::test]
    async fn aggregate_rejects_unknown_include_key() {
        let state = make_state();
        let request = Request::builder()
            .method(Method::GET)
            .uri("/agents")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .extension(AuthIdentity::operator())
            .body(Body::from(r#"{"include":["bogus"]}"#))
            .expect("static parts");
        let response = manage_router()
            .with_state(state)
            .oneshot(request)
            .await
            .unwrap_or_else(|infallible| match infallible {});
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn manage_router_serves_known_route() {
        let state = make_state();
        let response = manage_get(&state, "/budget").await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn manage_router_unknown_route_keeps_404_shape() {
        // A real tagma route that the management plane deliberately does NOT
        // expose (per-agent inbox): the subset boundary answers 404 with the
        // historical body shape.
        let state = make_state();
        let response =
            manage_get(&state, "/agents/00000000-0000-0000-0000-000000000000/inbox").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
            .await
            .expect("small body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(body["error"]["message"], "unknown management route");
    }

    #[tokio::test]
    async fn manage_relay_409_keeps_structured_dangling() {
        // The online shell keys its confirm flow off body.error.dangling;
        // pin that the manage channel relays the full envelope verbatim. A
        // flattened message-only body would regress the confirm dialog to
        // the raw error text (the acceptance failure).
        let state = make_state_two_sets();
        let _sub = alt_bound_sub(&state).await;
        let wire = serde_json::json!({
            "endpoints": { "test": { "id": "test", "family": "deepseek", "api_key": null, "base_url": null } },
            "sets": [{ "name": "default", "profiles": [{ "id": "test", "endpoint": "test", "model": "test", "max_context_window": 128000 }] }],
            "default": "default"
        });
        let request = Request::builder()
            .method(Method::PUT)
            .uri("/profiles")
            .extension(AuthIdentity::operator())
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(wire.to_string()))
            .expect("static parts");
        let response = manage_router()
            .with_state(state.clone())
            .oneshot(request)
            .await
            .unwrap_or_else(|infallible| match infallible {});
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
            .await
            .expect("small body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        let dangling = body["error"]["dangling"]
            .as_array()
            .expect("structured dangling list present");
        assert!(
            dangling
                .iter()
                .any(|s| s.as_str().unwrap_or_default().contains("'alt'"))
        );
    }

    #[tokio::test]
    async fn manage_router_exposes_operator_subset_only() {
        // Real tagma routes the management plane deliberately does NOT relay:
        // agent creation, approvals, and per-agent inbox/exec-policy/permissions
        // surfaces (the dirlock routes no longer exist). Each must 404 — the subset
        // boundary is a contract, not an accident of the route table.
        let state = make_state();
        let id = "00000000-0000-0000-0000-000000000000";
        let agent_scoped = [
            format!("/agents/{id}/permissions"),
            format!("/agents/{id}/exec-policy"),
            format!("/agents/{id}/inbox"),
            format!("/agents/{id}/inbox/summary"),
        ];
        let flat = [
            "/dirlocks",
            "/approvals",
            "/approvals/00000000-0000-0000-0000-000000000000",
        ];
        for uri in agent_scoped
            .iter()
            .map(String::as_str)
            .chain(flat.iter().copied())
        {
            let response = manage_get(&state, uri).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "uri: {uri}");
        }
    }

    /// Trailing-slash nail on the router side of the frame criteria: the
    /// manage router matches exact shapes, so the spelling the allowlist
    /// denies is a 404 here as well, and the two layers agree by
    /// construction. Status alone cannot separate the two 404 worlds -- the
    /// handler's own not-found is one of them -- so each spelling is pinned
    /// by its body: the registered shape reaches `remove_agent`, the
    /// trailing-slash spelling never leaves the router.
    #[tokio::test]
    async fn manage_router_trailing_slash_is_404() {
        let state = make_state();
        // Registered shape, unknown id: the request reaches the handler,
        // whose own not-found is a 404 too.
        let response = manage_call(&state, "DELETE", "/agents/a-1").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
            .await
            .expect("small body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(body["error"]["message"], "agent not found");
        // The same id with a trailing slash matches no route: the router's
        // own fallback answers.
        let response = manage_call(&state, "DELETE", "/agents/a-1/").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
            .await
            .expect("small body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(body["error"]["message"], "unknown management route");
    }

    #[tokio::test]
    async fn manage_router_query_is_lenient() {
        // The manage plane's historical query semantics: missing, empty, or
        // unparsable query strings yield the default struct (200), never a 400.
        let state = make_state();
        for uri in ["/agents", "/agents?", "/agents?bogus=%zz"] {
            let response = manage_get(&state, uri).await;
            assert_eq!(response.status(), StatusCode::OK, "uri: {uri}");
        }
        let response = manage_get(&state, "/agents?created_by=agent-1").await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    #[tokio::test]
    async fn manage_router_wrong_method_is_405() {
        // A registered path hit with a method the manage plane does not
        // mount (DELETE /budget) answers axum's 405 where the old string
        // table returned its uniform 404 — unreachable through the relay's
        // fixed op set, and the more correct status.
        let state = make_state();
        let request = Request::builder()
            .method(Method::DELETE)
            .uri("/budget")
            .extension(AuthIdentity::operator())
            .body(Body::empty())
            .expect("static parts");
        let response = manage_router()
            .with_state(state)
            .oneshot(request)
            .await
            .unwrap_or_else(|infallible| match infallible {});
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
