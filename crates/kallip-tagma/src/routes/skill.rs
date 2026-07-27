//! Skill discovery and management routes: paths and metadata.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{ApiError, SkillPathsResponse};
use kallip_runtime::tools::{skill_dir, skill_metadata};

use crate::state::SharedState;

/// GET /agents/{id}/skills/paths — return the shared skill directory path.
pub async fn skill_paths(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = state.registry.read().await;
    if !registry.contains_key(&id) {
        return Err(ApiError::not_found("agent not found"));
    }
    drop(registry);

    let shared = skill_dir()
        .map_err(ApiError::internal)?
        .to_string_lossy()
        .into_owned();

    Ok(Json(SkillPathsResponse { shared }))
}

/// GET /agents/{id}/skills/{name}/meta — return skill metadata.
pub async fn skill_meta(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path((id, skill_name)): Path<(AgentId, String)>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = state.registry.read().await;
    if !registry.contains_key(&id) {
        return Err(ApiError::not_found("agent not found"));
    }
    drop(registry);

    let meta = skill_metadata(&skill_name).map_err(|e| {
        let msg = e.to_string();
        // Validation failures (traversal, reserved name) are client errors;
        // anything else means the skill file is absent.
        if msg.contains("invalid skill name") || msg.contains("reserved skill name") {
            ApiError::bad_request(msg)
        } else {
            ApiError::not_found(format!("skill '{skill_name}' not found"))
        }
    })?;

    Ok(Json(meta))
}
