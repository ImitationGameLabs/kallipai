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

use super::RelayHandle;
use crate::auth::AuthIdentity;
use crate::routes::{agent, budget, context, profile_probe, profiles};
use crate::state::SharedState;
use crate::work_schedule;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt;
use kallip_common::protocol::ListAgentsQuery;
use kallip_lesche_common::message::TagmaReply;
use std::panic::AssertUnwindSafe;
use std::str::FromStr;
use tower::ServiceExt;
use tracing::{error, warn};
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

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
        .route("/agents/{id}/metadata", put(agent::update_metadata))
        .route(
            "/profiles",
            get(profiles::get_profiles).put(profiles::put_profiles),
        )
        .route("/profiles/probe", post(profile_probe::probe_profiles))
        .route("/profiles/apply", post(profiles::apply_profiles))
        .route(
            "/work-schedule",
            get(work_schedule::get_work_schedule).put(work_schedule::put_work_schedule),
        )
        .fallback(manage_fallback)
}

/// `list_agents` with the manage plane's historical query semantics: a
/// missing or unparsable query string yields the default struct (axum's
/// `Query` extractor would reject with 400 instead).
async fn list_agents_lenient(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    uri: Uri,
) -> Response {
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
        trace: &kallip_agora_common::ids::TraceId,
        req_id: u64,
        method: &str,
        path: &str,
        body: serde_json::Value,
    ) {
        let result = AssertUnwindSafe(self.dispatch_manage(method, path, body))
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

    async fn dispatch_manage(
        &self,
        method: &str,
        path: &str,
        body: serde_json::Value,
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
    use crate::test_helpers::make_state;

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
    async fn manage_router_exposes_operator_subset_only() {
        // Real tagma routes the management plane deliberately does NOT relay:
        // agent creation, approvals, dirlocks, and per-agent inbox/
        // exec-policy/permissions surfaces. Each must 404 — the subset
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
