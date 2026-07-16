mod agent;
mod budget;
mod dirlock;
mod restore;
pub use restore::restore_agents;
#[cfg(test)]
pub(crate) mod approval;
#[cfg(not(test))]
mod approval;
mod context;
mod message;
mod skill;
mod skill_promote;

use axum::Router;
use kallip_common::protocol::{ListAgentsResponse, ListApprovalsQuery, MessageRequest};
use state::SharedState;
use tower_http::cors::{AllowHeaders, Any, CorsLayer};

use crate::state;

/// Permissive CORS layer for the daemon. The daemon binds to localhost by
/// default and authenticates every request with a bearer operator token, so a
/// wildcard policy is safe here: CORS is not a security boundary, the operator
/// token is. This lets a browser-served frontend at a different origin (e.g. a
/// dev server on another port) call the HTTP API and open the authenticated SSE
/// stream. If the daemon is ever bound to a public interface, restrict the
/// origin instead.
///
/// `AllowHeaders::mirror_request()` reflects whatever the browser requests in
/// preflight, which is the wildcard semantics we want: unlike `Any` (the `*`
/// value), it actually covers `Authorization`, which the Fetch spec excludes
/// from `*`.
pub fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(AllowHeaders::mirror_request())
}

/// Build the full axum router with all agent routes.
pub fn router() -> Router<SharedState> {
    Router::new()
        .route(
            "/agents",
            axum::routing::post(agent::create_agent).get(agent::list_agents),
        )
        .route(
            "/agents/{id}/message",
            axum::routing::post(message::send_message),
        )
        .route(
            "/agents/{id}/events",
            axum::routing::get(message::sse_events),
        )
        .route("/agents/{id}", axum::routing::delete(agent::remove_agent))
        .route(
            "/agents/{id}/interrupt",
            axum::routing::post(agent::interrupt_agent),
        )
        .route(
            "/agents/{id}/status",
            axum::routing::get(context::agent_status),
        )
        .route(
            "/agents/{id}/permissions",
            axum::routing::get(context::agent_permissions),
        )
        .route(
            "/agents/{id}/exec-policy",
            axum::routing::get(context::get_exec_policy).put(context::update_exec_policy),
        )
        .route(
            "/agents/{id}/metadata",
            axum::routing::put(agent::update_metadata),
        )
        .route(
            "/agents/{id}/activity",
            axum::routing::put(agent::update_activity),
        )
        .route(
            "/agents/{id}/dirlocks",
            axum::routing::post(dirlock::acquire)
                .delete(dirlock::release)
                .get(dirlock::status),
        )
        .route("/dirlocks", axum::routing::get(dirlock::who))
        .route(
            "/budget",
            axum::routing::get(budget::get_budget).post(budget::update_budget),
        )
        .route("/approvals", axum::routing::get(approval::list_approvals))
        .route(
            "/approvals/{id}",
            axum::routing::get(approval::get_approval).post(approval::respond_approval),
        )
        .route(
            "/agents/{id}/skills/paths",
            axum::routing::get(skill::skill_paths),
        )
        .route(
            "/agents/{id}/skills/{name}/meta",
            axum::routing::get(skill::skill_meta),
        )
        .route(
            "/agents/{id}/skills/{name}/promote-request",
            axum::routing::post(skill_promote::submit_promote_request),
        )
        .route(
            "/skill-promote-requests",
            axum::routing::get(skill_promote::list_promote_requests),
        )
        .route(
            "/skill-promote-requests/{id}",
            axum::routing::get(skill_promote::show_promote_request)
                .post(skill_promote::respond_promote_request),
        )
}
