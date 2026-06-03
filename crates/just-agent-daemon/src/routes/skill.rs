//! Skill discovery and management routes: paths, metadata, and promote.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_common::agentid::AgentId;
use just_agent_common::protocol::{SkillPathsResponse, SkillPromoteRequest, SkillPromoteResponse};
use just_agent_runtime::tools::{promote_skill, skill_dir, skill_metadata};
use tracing::info;

use crate::state::SharedState;

/// GET /agents/{id}/skills/paths — return shared and agent-local skill directory paths.
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

/// POST /agents/{id}/skills/{name}/promote — copy a local skill to the shared directory.
pub async fn skill_promote(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path((id, skill_name)): Path<(AgentId, String)>,
    Json(req): Json<SkillPromoteRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Extract session_dir under the lock, then release before file I/O.
    let session_dir = {
        let registry = state.registry.read().await;
        registry.require_superior(auth.identity(), &id)?;
        let entry = registry
            .get(&id)
            .ok_or((StatusCode::NOT_FOUND, "agent not found".into()))?;
        entry.agent.session_dir.clone()
    };

    let shared = skill_dir();
    let destination = promote_skill(&skill_name, session_dir.as_deref(), &shared, req.force)
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("invalid skill name") || msg.contains("cannot promote") {
                (StatusCode::BAD_REQUEST, msg)
            } else if msg.contains("already exists") {
                (StatusCode::CONFLICT, msg)
            } else if msg.contains("not found")
                || msg.contains("no session directory")
                || msg.contains("no valid frontmatter")
            {
                (StatusCode::NOT_FOUND, msg)
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, msg)
            }
        })?;

    info!(id = %id, skill = %skill_name, force = req.force, "promoted skill");
    Ok((
        StatusCode::CREATED,
        Json(SkillPromoteResponse {
            name: skill_name,
            destination,
        }),
    ))
}
