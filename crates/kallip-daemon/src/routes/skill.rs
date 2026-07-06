//! Skill discovery and management routes: paths and metadata.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{ApiError, SkillPathsResponse};
use kallip_runtime::tools::{skill_dir, skill_metadata};

use crate::state::SharedState;

/// GET /agents/{id}/skills/paths — return shared and agent-local skill directory paths.
pub async fn skill_paths(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;

    let shared = skill_dir()
        .map_err(ApiError::internal)?
        .to_string_lossy()
        .into_owned();
    let local = entry
        .agent
        .agent_dir
        .as_ref()
        .map(|d| d.join("skills").to_string_lossy().into_owned());

    Ok(Json(SkillPathsResponse { shared, local }))
}

/// GET /agents/{id}/skills/{name}/meta — return skill metadata.
pub async fn skill_meta(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path((id, skill_name)): Path<(AgentId, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;

    let agent_dir = entry.agent.agent_dir.as_deref();
    let meta = skill_metadata(&skill_name, agent_dir).map_err(|e| {
        if e.to_string().contains("invalid skill name") {
            ApiError::bad_request(e.to_string())
        } else {
            ApiError::not_found(format!("skill '{skill_name}' not found"))
        }
    })?;

    Ok(Json(meta))
}
