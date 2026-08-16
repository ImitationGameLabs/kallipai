//! In-process management-op dispatch: route a TagmaControl::Manage to the
//! matching tagma route handler and emit the ManageResult.
//!
//! Mirrors how dispatch::execute_op calls routes::deliver_message with
//! Identity::Operator -- the relay IS the trusted operator-equivalent. Every
//! handler is called directly in-process; no HTTP loopback, no token, no SSRF.
//! The bilateral E2E conversation is the authorization boundary (same as
//! SendMessage). The single caller is the manage dispatch arm in
//! bilateral::handle_user_op; handle_manage is pub(super).

use std::panic::AssertUnwindSafe;
use futures_util::FutureExt;
use axum::body::to_bytes;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{ListAgentsQuery, TokenBudgetUpdateRequest};
use kallip_lesche_common::message::TagmaReply;
use tracing::{error, warn};
use crate::auth::AuthIdentity;
use crate::routes::{agent, budget, context, profiles};
use crate::work_schedule;
use super::RelayHandle;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

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
            .catch_unwind().await;
        let reply = match result {
            Ok(resp) => stamp_req_id(resp, req_id),
            Err(_) => {
                error!(req_id, "manage dispatch panicked; emitting 502");
                TagmaReply::ManageResult {
                    req_id, status: 502,
                    body: serde_json::json!({"error":{"message":"relay manage panicked"}}),
                }
            }
        };
        if let Err(e) = self.emit(trace, self.agent_sender(), reply, None).await {
            warn!(req_id, "manage emit: {e:#}");
        }
    }

    async fn dispatch_manage(
        &self, method: &str, path: &str, body: serde_json::Value,
    ) -> TagmaReply {
        let Some(state) = self.inner.state.upgrade() else {
            return TagmaReply::ManageResult {
                req_id: 0, status: 503,
                body: serde_json::json!({"error":{"message":"tagma shutting down"}}),
            };
        };
        // Split path and query string: online paths may carry `?key=val`.
        let (path_part, query_str) = match path.find('?') {
            Some(pos) => (&path[..pos], Some(&path[pos + 1..])),
            None => (path, None),
        };
        let segs: Vec<&str> = path_part.trim_start_matches('/').split('/').collect();
        let m = method.to_ascii_uppercase();
        let response = match (m.as_str(), segs.as_slice()) {

            ("GET", ["budget"]) => budget::get_budget(
                State(state.clone()), AuthIdentity::operator()).await.into_response(),
            ("POST", ["budget"]) => {
                let req = match serde_json::from_value::<TokenBudgetUpdateRequest>(body) {
                    Ok(r) => r, Err(e) => return bad_request(e),
                };
                budget::update_budget(
                    State(state.clone()), AuthIdentity::operator(), axum::Json(req))
                    .await.into_response()
            }
            ("GET", ["agents"]) => {
                let q = parse_query::<ListAgentsQuery>(query_str);
                agent::list_agents(
                    State(state.clone()), AuthIdentity::operator(), Query(q))
                    .await.into_response()
            }
            ("GET", ["agents", id, "status"]) => {
                let id = parse_agent_id(id);
                context::agent_status(
                    State(state.clone()), AuthIdentity::operator(), Path(id))
                    .await.into_response()
            }
            ("POST", ["agents", id, "interrupt"]) => {
                let id = parse_agent_id(id);
                agent::interrupt_agent(
                    State(state.clone()), AuthIdentity::operator(), Path(id))
                    .await.into_response()
            }
            ("DELETE", ["agents", id]) => {
                let id = parse_agent_id(id);
                agent::remove_agent(
                    State(state.clone()), AuthIdentity::operator(), Path(id))
                    .await.into_response()
            }
            ("PUT", ["agents", id, "duty"]) => {
                let id = parse_agent_id(id);
                let req = match serde_json::from_value::<agent::UpdateDutyRequest>(body) {
                    Ok(r) => r, Err(e) => return bad_request(e),
                };
                agent::update_duty(
                    State(state.clone()), AuthIdentity::operator(),
                    Path(id), axum::Json(req)).await.into_response()
            }
            ("PUT", ["agents", id, "metadata"]) => {
                let id = parse_agent_id(id);
                let req = match serde_json::from_value::<
                    kallip_common::protocol::UpdateAgentMetadataRequest>(body)
                { Ok(r) => r, Err(e) => return bad_request(e) };
                agent::update_metadata(
                    State(state.clone()), AuthIdentity::operator(),
                    Path(id), axum::Json(req)).await.into_response()
            }
            ("GET", ["profiles"]) => profiles::get_profiles(
                State(state.clone()), AuthIdentity::operator()).await.into_response(),
            ("PUT", ["profiles"]) => {
                let req = match serde_json::from_value::<
                    crate::routes::profiles::ProfileConfigWire>(body)
                { Ok(r) => r, Err(e) => return bad_request(e) };
                profiles::put_profiles(
                    State(state.clone()), AuthIdentity::operator(), axum::Json(req))
                    .await.into_response()
            }
            ("POST", ["profiles", "apply"]) => profiles::apply_profiles(
                State(state.clone()), AuthIdentity::operator()).await.into_response(),
            ("GET", ["work-schedules"]) => {
                let q = parse_query::<work_schedule::ListWorkSchedulesQuery>(query_str);
                work_schedule::list_work_schedules(
                    State(state.clone()), AuthIdentity::operator(), Query(q))
                    .await.into_response()
            }
            ("POST", ["work-schedules"]) => {
                let req = match serde_json::from_value::<
                    work_schedule::CreateWorkScheduleRequest>(body)
                { Ok(r) => r, Err(e) => return bad_request(e) };
                work_schedule::create_work_schedule(
                    State(state.clone()), AuthIdentity::operator(), axum::Json(req))
                    .await.into_response()
            }
            ("PUT", ["work-schedules", id]) => {
                let req = match serde_json::from_value::<
                    work_schedule::UpdateWorkScheduleRequest>(body)
                { Ok(r) => r, Err(e) => return bad_request(e) };
                work_schedule::update_work_schedule(
                    State(state.clone()), AuthIdentity::operator(),
                    Path(id.to_string()), axum::Json(req)).await.into_response()
            }
            ("DELETE", ["work-schedules", id]) => work_schedule::delete_work_schedule(
                State(state.clone()), AuthIdentity::operator(), Path(id.to_string()))
                .await.into_response(),
            _ => return TagmaReply::ManageResult {
                req_id: 0, status: 404,
                body: serde_json::json!({"error":{"message":"unknown management route"}}),
            },
        };
        extract_response(response).await
    }
}

fn parse_agent_id(s: &str) -> AgentId {
    // AgentId's FromStr is infallible (wraps any string), so this never fails.
    s.parse().unwrap()
}

fn bad_request(e: serde_json::Error) -> TagmaReply {
    TagmaReply::ManageResult {
        req_id: 0, status: 400,
        body: serde_json::json!({"error":{"message": format!("invalid request body: {e}")}}),
    }
}

/// Parse an optional query string into a typed query struct.
/// Returns the struct's default on parse failure or missing string.
fn parse_query<T: serde::de::DeserializeOwned + Default>(query_str: Option<&str>) -> T {
    query_str
        .and_then(|q| serde_urlencoded::from_str(q).ok())
        .unwrap_or_default()
}

fn stamp_req_id(mut reply: TagmaReply, req_id: u64) -> TagmaReply {
    if let TagmaReply::ManageResult { req_id: r, .. } = &mut reply { *r = req_id; }
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
                req_id: 0, status: 502,
                body: serde_json::json!({"error":{"message":"response body too large"}}),
            };
        }
    };
    let body_value: serde_json::Value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    TagmaReply::ManageResult { req_id: 0, status, body: body_value }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_common::protocol::ListAgentsQuery;
    use crate::work_schedule::WorkScheduleStatus;

    #[test]
    fn parse_query_none_returns_default() {
        let q: ListAgentsQuery = parse_query(None);
        assert!(q.created_by.is_none());
    }

    #[test]
    fn parse_query_empty_returns_default() {
        let q: ListAgentsQuery = parse_query(Some(""));
        assert!(q.created_by.is_none());
    }

    #[test]
    fn parse_query_parses_created_by() {
        let q: ListAgentsQuery = parse_query(Some("created_by=agent-1"));
        assert_eq!(q.created_by.unwrap().as_ref(), "agent-1");
    }

    #[test]
    fn parse_query_work_schedules_status() {
        let q: work_schedule::ListWorkSchedulesQuery =
            parse_query(Some("status=active"));
        assert_eq!(q.status, Some(WorkScheduleStatus::Active));
    }

    #[test]
    fn parse_query_work_schedules_agent_id() {
        let q: work_schedule::ListWorkSchedulesQuery =
            parse_query(Some("agent_id=agent-xyz"));
        assert_eq!(q.agent_id.unwrap().as_ref(), "agent-xyz");
    }

    #[test]
    fn path_split_strips_query_string() {
        let path = "/agents?created_by=foo";
        let (path_part, query_str) = match path.find('?') {
            Some(pos) => (&path[..pos], Some(&path[pos + 1..])),
            None => (path, None),
        };
        assert_eq!(path_part, "/agents");
        assert_eq!(query_str, Some("created_by=foo"));
        let segs: Vec<&str> = path_part.trim_start_matches('/').split('/').collect();
        assert_eq!(segs, vec!["agents"]);
    }

    #[test]
    fn path_split_no_query() {
        let path = "/budget";
        let (path_part, query_str) = match path.find('?') {
            Some(pos) => (&path[..pos], Some(&path[pos + 1..])),
            None => (path, None),
        };
        assert_eq!(path_part, "/budget");
        assert!(query_str.is_none());
        let segs: Vec<&str> = path_part.trim_start_matches('/').split('/').collect();
        assert_eq!(segs, vec!["budget"]);
    }

    #[test]
    fn path_split_nested_with_query() {
        let path = "/work-schedules?status=active&agent_id=x";
        let (path_part, query_str) = match path.find('?') {
            Some(pos) => (&path[..pos], Some(&path[pos + 1..])),
            None => (path, None),
        };
        assert_eq!(path_part, "/work-schedules");
        let segs: Vec<&str> = path_part.trim_start_matches('/').split('/').collect();
        assert_eq!(segs, vec!["work-schedules"]);
        let q: work_schedule::ListWorkSchedulesQuery = parse_query(query_str);
        assert_eq!(q.status, Some(WorkScheduleStatus::Active));
    }
}
