pub(crate) mod agent;
pub(crate) mod budget;
mod dirlock;
mod restore;
pub(crate) use agent::ensure_root_agent;
pub use restore::restore_agents;
#[cfg(test)]
pub(crate) mod approval;
#[cfg(not(test))]
mod approval;
pub(crate) mod context;
mod message;
pub(crate) mod profiles;
mod profile_probe;
/// The in-process message-delivery seam shared by the `send_message` route and
/// the relay's `execute_op`, plus its room inbound counterpart.
pub(crate) use message::{deliver_inbound_room_message, deliver_message, enqueue_prompt};
mod lesche;
mod inbox;

use axum::Router;
use kallip_common::protocol::{ListAgentsResponse, ListApprovalsQuery, MessageRequest};
use state::SharedState;
use tower_http::cors::{AllowHeaders, Any, CorsLayer};

use crate::state;

/// Permissive CORS layer for the tagma. The tagma binds to localhost by
/// default and authenticates every request with a bearer operator token, so a
/// wildcard policy is safe here: CORS is not a security boundary, the operator
/// token is. This lets a browser-served frontend at a different origin (e.g. a
/// dev server on another port) call the HTTP API and open the authenticated SSE
/// stream. If the tagma is ever bound to a public interface, restrict the
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
        .route("/agents/root", axum::routing::get(agent::get_root_agent))
        .route(
            "/agents/{id}/verify",
            axum::routing::get(agent::verify_agent),
        )
        .route(
            "/agents/{id}/message",
            axum::routing::post(message::send_message),
        )
        .route(
            "/agents/{id}/lesche/messages",
            axum::routing::post(lesche::post_message),
        )
        .route(
            "/agents/{id}/lesche/rooms",
            axum::routing::get(lesche::list_joined_rooms),
        )
        .route(
            "/agents/{id}/lesche/rooms/{room}/messages",
            axum::routing::get(lesche::read_room_messages),
        )
        .route(
            "/agents/{id}/events",
            axum::routing::get(message::sse_events),
        )
        .route(
            "/agents/{id}/external/events",
            axum::routing::get(message::external_events),
        )
        .route(
            "/agents/{id}/external/history",
            axum::routing::get(message::external_history),
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
            "/agents/{id}/duty",
            axum::routing::put(agent::update_duty),
        )
        .route(
            "/agents/{id}/inbox",
            axum::routing::get(inbox::list_inbox).delete(inbox::clear_inbox),
        )
        .route(
            "/agents/{id}/inbox/summary",
            axum::routing::get(inbox::inbox_summary),
        )
        .route(
            "/agents/{id}/inbox/{msg_id}",
            axum::routing::get(inbox::read_inbox_message).put(inbox::mark_done),
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
            "/profiles",
            axum::routing::get(profiles::get_profiles).put(profiles::put_profiles),
        )
        .route(
            "/profiles/apply",
            axum::routing::post(profiles::apply_profiles),
        )
        .route(
            "/profiles/probe",
            axum::routing::post(profile_probe::probe_profiles),
        )
        .route(
            "/work-schedules",
            axum::routing::get(crate::work_schedule::list_work_schedules)
                .post(crate::work_schedule::create_work_schedule),
        )
        .route(
            "/work-schedules/{id}",
            axum::routing::get(crate::work_schedule::get_work_schedule)
                .put(crate::work_schedule::update_work_schedule)
                .delete(crate::work_schedule::delete_work_schedule),
        )
}
