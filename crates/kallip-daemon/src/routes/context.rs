use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallip_common::agentid::AgentId;
use kallip_common::policy::{ExecPolicy, ToolPolicy};
use kallip_common::protocol::{AgentPermissionsResponse, AgentStatusResponse, ApiError};
use kallip_runtime::context::AgenticContext;
use kallip_runtime::persistence;
use kallip_runtime::policy::classifier;

use crate::state::SharedState;

/// GET /agents/{id}/status — return context usage and retry history.
/// Any authenticated identity may query any agent's status.
pub async fn agent_status(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
    // Take the brief std activity snapshot before the async store lock — no nesting.
    let activity = entry.agent.activity_snapshot();
    let store = entry.agent.store.lock().await;
    let context = store.usage_snapshot();
    let recent_retries = store
        .retry_log
        .iter()
        .rev()
        .take(20)
        .cloned()
        .collect::<Vec<_>>();
    let snap = state.token_budget.snapshot();
    Ok(Json(AgentStatusResponse {
        state: entry.agent.get_state(),
        context,
        recent_retries,
        token_budget: snap.budget,
        token_consumed: snap.consumed,
        activity,
    }))
}

/// GET /agents/{id}/permissions — return agent permission profile and tool policy.
/// Any authenticated identity may query any agent's permissions.
pub async fn agent_permissions(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
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
) -> Result<impl IntoResponse, ApiError> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
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
) -> Result<StatusCode, ApiError> {
    let registry = state.registry.read().await;
    registry.require_superior(auth.identity(), &id)?;

    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;

    // Strictness validation against parent.
    if let Some(ref parent_id) = entry.agent.config.created_by {
        let parent_entry = registry
            .get(parent_id)
            .ok_or_else(|| ApiError::internal("parent agent not found"))?;
        let parent_policy = parent_entry
            .agent
            .tool_policy
            .read()
            .unwrap_or_else(|e| e.into_inner());
        new_policy
            .validate_at_least_as_strict_as(&parent_policy)
            .map_err(|violations| {
                ApiError::conflict(format!("policy violations: {}", violations.join("; ")))
            })?;
    }

    // Cascade: children must still be at least as strict as the new policy.
    for child_id in &entry.subagent_ids {
        let child = registry
            .get(child_id)
            .ok_or_else(|| ApiError::internal(format!("child agent {child_id} not found")))?;
        let child_policy = child
            .agent
            .tool_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        child_policy
            .validate_at_least_as_strict_as(&new_policy)
            .map_err(|violations| {
                ApiError::conflict(format!("child {child_id}: {}", violations.join("; ")))
            })?;
    }

    // Persist first, then update in-memory.
    let agent_dir = entry
        .agent
        .agent_dir
        .as_ref()
        .ok_or_else(|| ApiError::internal("agent has no persistent directory"))?;
    persistence::persist_policy(agent_dir, &new_policy).map_err(ApiError::internal)?;

    *entry
        .agent
        .tool_policy
        .write()
        .unwrap_or_else(|e| e.into_inner()) = new_policy;

    Ok(StatusCode::NO_CONTENT)
}

/// GET /agents/{id}/exec-policy — return the `bash_exec` command-policy overrides.
pub async fn get_exec_policy(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
    let exec_policy = entry
        .agent
        .exec_policy
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Ok(Json(exec_policy))
}

/// PUT /agents/{id}/exec-policy — update the `bash_exec` overrides with
/// monotonic-strictness validation.
///
/// # Lock ordering
///
/// Same discipline as [`update_policy`]: registry async read lock first, then
/// per-agent `std::sync::RwLock<ExecPolicy>`. `exec_policy` and `tool_policy`
/// are independent lock classes; a handler acquiring both must take
/// `tool_policy` before `exec_policy`.
pub async fn update_exec_policy(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
    Json(mut new_policy): Json<ExecPolicy>,
) -> Result<StatusCode, ApiError> {
    let registry = state.registry.read().await;
    registry.require_superior(auth.identity(), &id)?;

    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;

    // Normalize keys (command names are matched case-insensitively), then reject
    // keys that would be silent no-ops (interpreter/eval names).
    new_policy.lowercase_keys();
    for name in new_policy.overrides.keys() {
        classifier::is_valid_override_key(name).map_err(ApiError::bad_request)?;
    }

    // Strictness against parent — effective decisions vs the catalog baseline.
    if let Some(ref parent_id) = entry.agent.config.created_by {
        let parent_entry = registry
            .get(parent_id)
            .ok_or_else(|| ApiError::internal("parent agent not found"))?;
        let parent_policy = parent_entry
            .agent
            .exec_policy
            .read()
            .unwrap_or_else(|e| e.into_inner());
        new_policy
            .validate_at_least_as_strict_as(&parent_policy, classifier::exec_baseline)
            .map_err(|violations| {
                ApiError::conflict(format!("exec_policy violations: {}", violations.join("; ")))
            })?;
    }

    // Cascade: children must still be at least as strict.
    for child_id in &entry.subagent_ids {
        let child = registry
            .get(child_id)
            .ok_or_else(|| ApiError::internal(format!("child agent {child_id} not found")))?;
        let child_policy = child
            .agent
            .exec_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        child_policy
            .validate_at_least_as_strict_as(&new_policy, classifier::exec_baseline)
            .map_err(|violations| {
                ApiError::conflict(format!("child {child_id}: {}", violations.join("; ")))
            })?;
    }

    // Persist first, then update in-memory.
    let agent_dir = entry
        .agent
        .agent_dir
        .as_ref()
        .ok_or_else(|| ApiError::internal("agent has no persistent directory"))?;
    persistence::persist_exec_policy(agent_dir, &new_policy).map_err(ApiError::internal)?;

    *entry
        .agent
        .exec_policy
        .write()
        .unwrap_or_else(|e| e.into_inner()) = new_policy;

    Ok(StatusCode::NO_CONTENT)
}
