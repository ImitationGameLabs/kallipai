mod agent;
pub use agent::restore_sessions;
mod approval;
mod context;
mod message;

use axum::Router;
use serde::{Deserialize, Serialize};
use state::SharedState;

use crate::state;
use just_agent_core::types::{CreateAgentRequest, CreateAgentResponse};

#[derive(Debug, Serialize)]
pub struct ListAgentsResponse {
    pub agents: Vec<state::AgentSummary>,
}

#[derive(Debug, Deserialize)]
pub struct MessageRequest {
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub decision: String,
    pub reason: Option<String>,
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
        .route(
            "/agents/{id}/approval",
            axum::routing::post(approval::respond_approval),
        )
        .route("/agents/{id}", axum::routing::delete(agent::delete_agent))
        .route(
            "/agents/{id}/interrupt",
            axum::routing::post(agent::interrupt_agent),
        )
        .route(
            "/agents/{id}/status",
            axum::routing::get(context::agent_status),
        )
}
