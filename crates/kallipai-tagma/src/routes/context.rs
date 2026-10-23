use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use kallipai_adk::context::AgenticContext;
use kallipai_adk::persistence;
use kallipai_adk::policy::classifier;
use kallipai_common::agentid::AgentId;
use kallipai_common::facts::ProfilePacing;
use kallipai_common::policy::ExecPolicy;
use kallipai_common::protocol::{
    ActiveProfile, AgentPermissionsResponse, AgentStatusResponse, AgentUsageStats, ApiError,
    TagmaUsageTotals,
};
use kallipai_common::retry::RETRY_LOG_KEEP;

use crate::state::SharedState;

/// GET /agents/{id}/status — return context usage and retry history.
/// Any authenticated identity may query any agent's status — the task list is operator/root only.
pub async fn agent_status(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<AgentId>,
) -> Result<impl IntoResponse, ApiError> {
    // Resolve the entitlement before taking the registry lock: the predicate
    // takes a read lock of its own, and nesting the two would deadlock against
    // a writer waiting in between.
    let bg_tasks_visible = crate::auth::is_operator_or_root(&state, auth.identity()).await;
    let registry = state.registry.read().await;
    let entry = registry
        .get(&id)
        .ok_or_else(|| ApiError::not_found("agent not found"))?;
    let live = entry
        .as_live()
        .ok_or_else(|| ApiError::conflict("agent is faulted; no status"))?;
    // Take the brief std snapshots before the async store lock — no nesting.
    let activity = live.agent.activity_snapshot();
    let profile_snap = live.agent.active_profile_snapshot();
    let bg_tasks = bg_tasks_visible.then(|| live.agent.bg_tasks_snapshot());
    let store = live.agent.store.lock().await;
    let context = store.usage_snapshot();
    let recent_retries = store
        .retry_log
        .iter()
        .rev()
        .take(RETRY_LOG_KEEP)
        .cloned()
        .collect::<Vec<_>>();
    let snap = state.token_budget.snapshot();
    // Single-launch usage: the per-agent counters this agent recorded, plus
    // tagma-wide totals. Both hit rates are computed here so clients never
    // re-derive (and can never mis-derive) the sum-then-divide ratio.
    let usage_entry = state.usage_stats.agent(id);
    let usage = usage_entry.map(|u| AgentUsageStats {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        cache_read_tokens: u.cache_read_tokens,
        cache_hit_rate: u.cache_hit_rate(),
    });
    let totals = state.usage_stats.totals();
    let usage_totals = TagmaUsageTotals {
        prompt_tokens: totals.prompt_tokens,
        completion_tokens: totals.completion_tokens,
        cache_read_tokens: totals.cache_read_tokens,
        cache_hit_rate: totals.cache_hit_rate(),
    };
    let upstream_facts = state
        .profile_pacing
        .snapshot(kallipai_common::timefmt::now_epoch());
    Ok(Json(AgentStatusResponse {
        state: live.agent.get_state(),
        context,
        recent_retries,
        token_budget: snap.budget,
        token_consumed: snap.consumed,
        token_budget_unlimited: snap.unlimited,
        activity,
        parked_reason: live.agent.parked_reason_snapshot(),
        retrying: live.agent.retrying_snapshot(),
        profile: Some(ActiveProfile {
            profile_set: profile_snap.set_name,
            profile_id: profile_snap.profile_id,
            provider: profile_snap.provider,
            model: profile_snap.model,
        }),
        usage,
        usage_totals: Some(usage_totals),
        upstream_facts,
        background_tasks: bg_tasks,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthIdentity, Identity};
    use crate::test_helpers::{add_root, add_sub, make_state};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// The status surface must carry the runtime-active profile from the
    /// shared cell: seed the cell directly (only the runtime's FailoverState
    /// writes it) and assert the wire shape end-to-end through the handler.
    #[tokio::test]
    async fn status_carries_runtime_active_profile() {
        let state = make_state();
        let id = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &id);
        }
        {
            let reg = state.registry.read().await;
            let live = reg.get(&id).unwrap().as_live().unwrap();
            *live.agent.profile_snapshot.lock().unwrap() = kallipai_adk::ProfileSnapshot {
                set_name: "set1".into(),
                provider: "oc-go".into(),
                profile_id: "set1-deepseek".into(),
                model: "deepseek-chat".into(),
            };
        }
        let resp = agent_status(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path(id),
        )
        .await
        .unwrap();
        let body = axum::body::to_bytes(
            axum::response::IntoResponse::into_response(resp).into_body(),
            usize::MAX,
        )
        .await
        .unwrap();
        let status: AgentStatusResponse = serde_json::from_slice(&body).unwrap();
        let profile = status.profile.expect("profile present");
        assert_eq!(profile.profile_id, "set1-deepseek");
        assert_eq!(profile.profile_set, "set1");
        assert_eq!(profile.model, "deepseek-chat");
        assert_eq!(profile.provider, "oc-go");
    }

    /// Call the status handler as `identity` and return the raw response body:
    /// the wire bytes are what a client actually consumes.
    async fn status_body(state: &SharedState, identity: Identity, id: &AgentId) -> Vec<u8> {
        let resp = agent_status(
            State(state.clone()),
            AuthIdentity::test_new(identity),
            Path(id.clone()),
        )
        .await
        .unwrap();
        axum::body::to_bytes(
            axum::response::IntoResponse::into_response(resp).into_body(),
            usize::MAX,
        )
        .await
        .unwrap()
        .to_vec()
    }

    /// The same call, decoded.
    async fn status_as(
        state: &SharedState,
        identity: Identity,
        id: &AgentId,
    ) -> AgentStatusResponse {
        serde_json::from_slice(&status_body(state, identity, id).await).unwrap()
    }

    /// Seed one running task into the agent's host-side cell (only the shell
    /// publishes there at runtime), with the cwd recorded verbatim. Returns
    async fn seed_task(state: &SharedState, id: &AgentId) -> (Arc<AtomicBool>, Arc<AtomicUsize>) {
        let stalled = Arc::new(AtomicBool::new(false));
        let bytes = Arc::new(AtomicUsize::new(4_198));
        let reg = state.registry.read().await;
        let live = reg.get(id).unwrap().as_live().unwrap();
        live.agent
            .bg_tasks
            .lock()
            .unwrap()
            .push(kallipai_shell::BgTaskHandle {
                id: "9f2c1a04".to_string(),
                stalled: stalled.clone(),
                bytes: bytes.clone(),
                command: "cargo test --workspace --all-features".to_string(),
                cwd: std::path::PathBuf::from("~/work"),
                started_at: 1_000,
            });
        (stalled, bytes)
    }

    /// Empty the cell again, so one caller's body can be compared across the
    /// two states.
    async fn clear_tasks(state: &SharedState, id: &AgentId) {
        let reg = state.registry.read().await;
        let live = reg.get(id).unwrap().as_live().unwrap();
        live.agent.bg_tasks.lock().unwrap().clear();
    }

    /// The host-side task list reaches the two callers entitled to it: the root
    /// agent itself and the operator.
    #[tokio::test]
    async fn background_tasks_reach_the_operator_and_the_root_agent() {
        let state = make_state();
        let root = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &root);
        }
        seed_task(&state, &root).await;

        for identity in [Identity::Agent { id: root.clone() }, Identity::Operator] {
            let status = status_as(&state, identity, &root).await;
            let tasks = status.background_tasks.expect("tasks are present");
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].id, "9f2c1a04");
            assert!(!tasks[0].stalled);
            assert_eq!(tasks[0].bytes, 4198);
            assert_eq!(tasks[0].command, "cargo test --workspace --all-features");
            assert_eq!(tasks[0].cwd, "~/work");
            assert_eq!(tasks[0].started_at, 1_000);
        }
    }

    /// Any other caller — a plain agent included, and even one reading its own
    /// status — receives the body a tagma without this field would send.
    #[tokio::test]
    async fn background_tasks_are_absent_for_every_other_caller() {
        let state = make_state();
        let root = AgentId::random();
        let sub = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &root);
            add_sub(&mut reg, &sub, &root);
        }
        seed_task(&state, &root).await;

        let identity = Identity::Agent { id: sub.clone() };
        let status = status_as(&state, identity.clone(), &root).await;
        assert!(status.background_tasks.is_none());

        // Wire-level: seeding the tasks changes nothing for this caller.
        let with_tasks = status_body(&state, identity.clone(), &root).await;
        clear_tasks(&state, &root).await;
        let cleared = status_body(&state, identity, &root).await;
        assert_eq!(with_tasks, cleared);
    }

    /// The cell carries live counters, not a frozen frame: they are the same
    /// atomics a watcher moves, so a status read reports the task's current
    /// output size and stall flag rather than the values from the last publish.
    #[tokio::test]
    async fn background_tasks_report_live_counters() {
        let state = make_state();
        let root = AgentId::random();
        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &root);
        }
        let (stalled, bytes) = seed_task(&state, &root).await;
        let before = status_as(&state, Identity::Operator, &root).await;
        assert_eq!(before.background_tasks.unwrap()[0].bytes, 4_198);

        bytes.store(9_001, Ordering::Relaxed);
        stalled.store(true, Ordering::Relaxed);

        let after = status_as(&state, Identity::Operator, &root).await;
        let tasks = after.background_tasks.expect("tasks are present");
        assert_eq!(
            tasks[0].bytes, 9_001,
            "the byte count is read at snapshot time"
        );
        assert!(tasks[0].stalled, "the stall flag is read at snapshot time");
    }
}
