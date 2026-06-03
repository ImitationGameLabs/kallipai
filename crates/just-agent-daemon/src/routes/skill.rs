//! Skill discovery routes: paths and metadata.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_common::agentid::AgentId;
use just_agent_common::protocol::SkillPathsResponse;
use just_agent_runtime::tools::{skill_dir, skill_metadata};

use crate::state::SharedState;

/// GET /agents/{id}/skill-paths — return shared and agent-local skill directory paths.
pub async fn skill_paths(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "agent not found".into()))?;

    let shared = skill_dir().to_string_lossy().into_owned();
    let local = entry
        .agent
        .session_dir
        .as_ref()
        .map(|d| d.join("skills").to_string_lossy().into_owned());

    Ok(Json(SkillPathsResponse { shared, local }))
}

/// GET /agents/{id}/skills/{name}/meta — return skill metadata.
pub async fn skill_meta(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path((id, skill_name)): Path<(AgentId, String)>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "agent not found".into()))?;

    let session_dir = entry.agent.session_dir.as_deref();
    let meta = skill_metadata(&skill_name, session_dir).map_err(|e| {
        if e.to_string().contains("invalid skill name") {
            (StatusCode::BAD_REQUEST, e.to_string())
        } else {
            (
                StatusCode::NOT_FOUND,
                format!("skill '{skill_name}' not found"),
            )
        }
    })?;

    Ok(Json(meta))
}
