use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallip_common::agentid::AgentId;
use kallip_common::policy::ExecPolicy;
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
    let live = entry
        .as_live()
        .ok_or_else(|| ApiError::conflict("agent is faulted; no status"))?;
    // Take the brief std activity snapshot before the async store lock — no nesting.
    let activity = live.agent.activity_snapshot();
    let store = live.agent.store.lock().await;
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
        state: live.agent.get_state(),
        context,
        recent_retries,
        token_budget: snap.budget,
        token_consumed: snap.consumed,
        activity,
    }))
}

/// GET /agents/{id}/permissions — return agent permission profile and the active
/// classify preset. Any authenticated identity may query any agent's permissions.
pub async fn agent_permissions(
    State(state): State<SharedState>,
    _auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
    let config = &entry.identity().config;
    let live = entry
        .as_live()
        .ok_or_else(|| ApiError::conflict("agent is faulted; permissions not available"))?;
    Ok(Json(AgentPermissionsResponse {
        max_depth: config.permissions.max_depth,
        workspace_root: config.workspace_root.to_string_lossy().into_owned(),
        created_by: config.created_by.clone(),
        // The tagma-global classify preset this agent runs under (immutable for
        // the tagma's lifetime).
        preset: live.agent.preset,
        // Lowercase wire spelling via Display — the value the tagma clamped at
        // spawn and re-validates on restore, surfaced so an explicit downgrade
        // is observable.
        permission_class: config.permissions_class.to_string(),
    }))
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
    let live = entry
        .as_live()
        .ok_or_else(|| ApiError::conflict("agent is faulted; exec_policy not available"))?;
    let exec_policy = live
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
/// Registry async read lock first, then the per-agent `std::sync::RwLock<ExecPolicy>`.
/// `exec_policy` is now the only per-agent policy lock (the classify preset is an
/// immutable snapshot, not a lock).
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
    let live = entry
        .as_live()
        .ok_or_else(|| ApiError::conflict("agent is faulted; exec_policy not available"))?;

    // Normalize keys (command names are matched case-insensitively), then reject
    // keys that would be silent no-ops (interpreter/eval names).
    new_policy.lowercase_keys();
    for name in new_policy.overrides.keys() {
        classifier::is_valid_override_key(name).map_err(ApiError::bad_request)?;
    }

    // Strictness against parent — effective decisions vs the catalog baseline.
    // A faulted parent contributes no constraint; skip it.
    if let Some(ref parent_id) = entry.identity().config.created_by
        && let Some(parent_entry) = registry.get(parent_id)
        && let Some(parent_live) = parent_entry.as_live()
    {
        let parent_policy = parent_live
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

    // Cascade: children must still be at least as strict. Skip faulted children.
    for child_id in entry.subagent_ids() {
        let Some(child) = registry.get(child_id) else {
            continue;
        };
        let Some(child_live) = child.as_live() else {
            continue;
        };
        let child_policy = child_live
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
    let agent_dir = live
        .identity
        .agent_dir
        .as_ref()
        .ok_or_else(|| ApiError::internal("agent has no persistent directory"))?;
    persistence::persist_exec_policy(agent_dir, &new_policy).map_err(ApiError::internal)?;

    *live
        .agent
        .exec_policy
        .write()
        .unwrap_or_else(|e| e.into_inner()) = new_policy;

    Ok(StatusCode::NO_CONTENT)
}
