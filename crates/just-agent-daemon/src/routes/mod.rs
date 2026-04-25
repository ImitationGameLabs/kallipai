mod agent;
mod approval;
mod context;
mod prompt;

use axum::Router;
use serde::Deserialize;
use state::SharedState;

use crate::state;

#[derive(Debug, Deserialize)]
pub struct CreateAgentRequest {
    pub workspace_root: Option<String>,
    pub skills: Vec<String>,
    pub prompt: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListAgentsResponse {
    pub agents: Vec<state::AgentSummary>,
}

#[derive(Debug, Deserialize)]
pub struct PromptRequest {
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub decision: String,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateAgentResponse {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct SkillRequest {
    pub name: String,
}

use serde::Serialize;

/// Build the full axum router with all agent routes.
pub fn router() -> Router<SharedState> {
    Router::new()
        .route(
            "/agents",
            axum::routing::post(agent::create_agent).get(agent::list_agents),
        )
        .route(
            "/agents/{id}/prompt",
            axum::routing::post(prompt::send_prompt),
        )
        .route(
            "/agents/{id}/events",
            axum::routing::get(prompt::sse_events),
        )
        .route(
            "/agents/{id}/approval",
            axum::routing::post(approval::respond_approval),
        )
        .route("/agents/{id}", axum::routing::delete(agent::delete_agent))
        .route(
            "/agents/{id}/status",
            axum::routing::get(context::agent_status),
        )
        .route(
            "/agents/{id}/compact",
            axum::routing::post(context::agent_compact),
        )
        .route(
            "/agents/{id}/skill",
            axum::routing::post(context::agent_load_skill),
        )
}
