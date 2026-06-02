use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use just_agent_common::context::ContextUsage;
use just_agent_common::retry::RetryRecord;
use just_agent_common::types::AgentId;
use just_agent_common::types::AgentPermissionsResponse;
use just_agent_common::types::AgentState;
use just_agent_common::types::ToolPolicy;
use just_agent_runtime::context::AgenticContext;
use just_agent_runtime::persistence;

use crate::state::SharedState;
use serde::Serialize;

/// Combined status response: context usage + recent retry history.
#[derive(Serialize)]
pub struct AgentStatus {
    pub state: AgentState,
    pub context: ContextUsage,
    pub recent_retries: Vec<RetryRecord>,
}

/// GET /agents/{id}/status — return context usage and retry history.
/// Any authenticated identity may query any agent's status.
pub async fn agent_status(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "agent not found".into()))?;
    let store = entry.agent.store.lock().await;
    let context = store.usage_snapshot();
    let recent_retries = store
        .retry_log
        .iter()
        .rev()
        .take(20)
        .cloned()
        .collect::<Vec<_>>();
    Ok(Json(AgentStatus {
        state: entry.agent.get_state(),
        context,
        recent_retries,
    }))
}

/// GET /agents/{id}/permissions — return agent permission profile and tool policy.
/// Any authenticated identity may query any agent's permissions.
pub async fn agent_permissions(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "agent not found".into()))?;
    let config = &entry.agent.config;
    let tool_policy = entry
        .agent
        .tool_policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Ok(Json(AgentPermissionsResponse {
        max_depth: config.permissions.max_depth,
        workspace_root: config.workspace_root.to_string_lossy().into_owned(),
        created_by: config.created_by.clone(),
        tool_policy,
    }))
}

/// GET /agents/{id}/policy — return the raw tool policy.
pub async fn get_policy(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "agent not found".into()))?;
    let policy = entry
        .agent
        .tool_policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Ok(Json(policy))
}

/// PUT /agents/{id}/policy — update tool policy with strictness validation.
///
/// # Lock ordering
///
/// This handler acquires locks in a strict order to prevent deadlocks:
///
/// 1. **Registry async read lock** — held throughout to look up parent, children,
///    and the target entry. Released on return.
/// 2. **Per-agent `std::sync::RwLock<ToolPolicy>`** — read locks on parent and
///    children for strictness/cascade validation, write lock on target for the update.
///
/// Because `evaluate()` (runtime) only acquires tool_policy **read** locks and never
/// touches the registry, there is no circular dependency between the two lock classes.
/// A future refactor that needs both must acquire the registry lock first.
pub async fn update_policy(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(new_policy): Json<ToolPolicy>,
) -> Result<StatusCode, (StatusCode, String)> {
    let registry = state.registry.read().await;
    registry.require_superior(auth.identity(), &id)?;

    let entry = registry
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, "agent not found".into()))?;

    // Strictness validation against parent.
    if let Some(ref parent_id) = entry.agent.config.created_by {
        let parent_entry = registry.get(parent_id).ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "parent agent not found".into(),
        ))?;
        let parent_policy = parent_entry
            .agent
            .tool_policy
            .read()
            .unwrap_or_else(|e| e.into_inner());
        new_policy
            .validate_at_least_as_strict_as(&parent_policy)
            .map_err(|violations| {
                (
                    StatusCode::CONFLICT,
                    format!("policy violations: {}", violations.join("; ")),
                )
            })?;
    }

    // Cascade: children must still be at least as strict as the new policy.
    for child_id in &entry.subagent_ids {
        let child = registry.get(child_id).ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("child agent {child_id} not found"),
        ))?;
        let child_policy = child
            .agent
            .tool_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        child_policy
            .validate_at_least_as_strict_as(&new_policy)
            .map_err(|violations| {
                (
                    StatusCode::CONFLICT,
                    format!("child {child_id}: {}", violations.join("; ")),
                )
            })?;
    }

    // Persist first, then update in-memory.
    let session_dir = entry.agent.session_dir.as_ref().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "agent has no session directory".into(),
    ))?;
    persistence::persist_policy(session_dir, &new_policy)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    *entry
        .agent
        .tool_policy
        .write()
        .unwrap_or_else(|e| e.into_inner()) = new_policy;

    Ok(StatusCode::NO_CONTENT)
}
