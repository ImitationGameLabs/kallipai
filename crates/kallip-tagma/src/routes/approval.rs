use axum::Json;
use axum::extract::{Path, Query, State};
use kallip_common::approval::ApprovalStatus;
use kallip_common::protocol::{
    ApiError, ApprovalDecisionBody, ApprovalEntry, ListApprovalsResponse, SseEvent,
};
use kallip_runtime::persistence;
use kallip_runtime::policy::classifier::Classifier;
use kallip_runtime::{names, policy::ToolDecision};

use super::ListApprovalsQuery;
use crate::state::SharedState;

/// Maximum number of items returned by a single list request.
///
/// Client `limit` values are clamped to `[1, MAX_PAGE_SIZE]`.
const MAX_PAGE_SIZE: usize = 20;

pub async fn list_approvals(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Query(params): Query<ListApprovalsQuery>,
) -> Result<Json<ListApprovalsResponse>, ApiError> {
    let registry = state.registry.read().await;

    let mut entries: Vec<ApprovalEntry> = Vec::new();
    for (agent_id, entry) in registry.iter() {
        // Faulted entries have no approval queue and no task to act on a
        // decision; skip them (defense-in-depth, and required for compilation
        // since `approvals` is a live-only field).
        let Some(live) = entry.as_live() else {
            continue;
        };
        if registry
            .require_superior(auth.identity(), agent_id)
            .is_err()
        {
            continue;
        }
        if let Some(ref filter_agent) = params.requested_by
            && agent_id != filter_agent
        {
            continue;
        }
        let q = live.agent.approvals.lock().await;
        for info in q.list(None) {
            // Pending actions are not yet committed — not visible to superiors.
            if info.status == ApprovalStatus::Pending {
                continue;
            }
            entries.push(ApprovalEntry::from_info(
                info.id,
                agent_id.clone(),
                info.content,
                info.commit_reason,
                info.status,
                info.deny_reason,
                info.created_at,
            ));
        }
    }
    drop(registry);

    // Filter by status.
    if let Some(ref status_str) = params.status
        && let Some(filter) = ApprovalStatus::from_str_name(status_str)
    {
        entries.retain(|e| e.status == filter);
    }

    // Sort by created_at with configurable direction.
    let descending = params.order.as_deref() != Some("asc");
    entries.sort_by(|a, b| {
        if descending {
            b.created_at.cmp(&a.created_at)
        } else {
            a.created_at.cmp(&b.created_at)
        }
    });

    let total = entries.len();
    let offset: usize = match params.offset.unwrap_or(0).try_into() {
        Ok(v) => v,
        Err(e) => return Err(ApiError::bad_request(format!("invalid offset: {e}"))),
    };
    let limit = params.limit.unwrap_or(5).clamp(1, MAX_PAGE_SIZE as u64) as usize;
    let items: Vec<_> = entries.into_iter().skip(offset).take(limit).collect();

    Ok(Json(ListApprovalsResponse { items, total }))
}

pub async fn get_approval(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
) -> Result<Json<ApprovalEntry>, ApiError> {
    let registry = state.registry.read().await;
    for (agent_id, entry) in registry.iter() {
        let Some(live) = entry.as_live() else {
            continue;
        };
        let approvals = live.agent.approvals.lock().await;
        if let Some(info) = approvals.get(&id) {
            registry.require_superior(auth.identity(), agent_id)?;
            return Ok(Json(ApprovalEntry::from_info(
                info.id,
                agent_id.clone(),
                info.content,
                info.commit_reason,
                info.status,
                info.deny_reason,
                info.created_at,
            )));
        }
    }
    Err(ApiError::not_found("approval not found"))
}

/// Approve or deny a pending approval request.
///
/// # Authorization
///
/// The caller must be a superior of the agent that owns the approval
/// (checked via the [`AgentRegistry](crate::state::AgentRegistry)'s `require_superior` method).
///
/// For **approve** decisions, an additional gate applies when the deferred action
/// is a `bash_exec`: the superior's own classify rule-set (the tagma-global
/// preset plus the superior's exec-policy overrides) must classify the command
/// as `Allow`. This prevents a superior from using subordinates as proxies to run
/// a command its own policy would gate. Only `bash_exec` can defer today (every
/// other tool is unconditional `Allow`), so non-`bash_exec` actions need no gate.
/// The operator identity is exempt.
///
/// For **deny** decisions, no policy check is required — any superior may deny.
pub async fn respond_approval(
    State(state): State<SharedState>,
    auth: crate::auth::AuthIdentity,
    Path(id): Path<String>,
    Json(req): Json<ApprovalDecisionBody>,
) -> Result<axum::http::StatusCode, ApiError> {
    let registry = state.registry.read().await;

    // Find the owning agent and apply the decision in a single approval-lock
    // acquisition to prevent TOCTOU races with the agent loop.
    for (agent_id, entry) in registry.iter() {
        // Skip faulted entries up front: they have no approval queue to lock
        // and no task to notify. Must precede the `approvals.lock()` below.
        let Some(live) = entry.as_live() else {
            continue;
        };
        let mut approvals = live.agent.approvals.lock().await;
        if !approvals.contains(&id) {
            continue;
        }

        registry.require_superior(auth.identity(), agent_id)?;

        let info = approvals.get(&id).expect("contains checked above");

        let json = match req.decision.as_str() {
            "approve" => {
                // Anti-proxy-bypass gate: a superior approving a deferred
                // `bash_exec` may do so only if its own classify rule-set
                // (tagma-global preset + its exec-policy overrides) would Allow
                // the command. Operator is exempt. Non-bash_exec actions have no
                // security surface and need no gate.
                if info.content.tool_name == names::BASH_EXEC
                    && let crate::auth::Identity::Agent { id: caller_id } = auth.identity()
                {
                    // Extract the command from the deferred bash_exec arguments.
                    let command = info
                        .content
                        .arguments
                        .get("command")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            ApiError::bad_request(
                                "cannot approve bash_exec: deferred action has no 'command' \
                                 argument",
                            )
                        })?;
                    let caller_entry = registry
                        .get(caller_id)
                        .ok_or_else(|| ApiError::internal("caller agent not found in registry"))?;
                    // A faulted agent cannot authenticate (never token-indexed),
                    // so the caller is always live; reject defensively anyway.
                    let caller_live = caller_entry
                        .as_live()
                        .ok_or_else(|| ApiError::internal("caller agent is faulted"))?;
                    let exec_policy = caller_live
                        .agent
                        .exec_policy
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let decision =
                        Classifier::DEFAULT.classify_with(command, &exec_policy, state.preset);
                    if !matches!(decision, ToolDecision::Allow) {
                        return Err(ApiError::forbidden(format!(
                            "cannot approve bash_exec: caller's classify rule-set does not \
                             allow the command ({decision:?}); a superior may not delegate a \
                             command its own policy would gate",
                        )));
                    }
                }

                approvals
                    .approve(&id)
                    .map_err(|e| ApiError::conflict(e.to_string()))?;
                live.agent
                    .events_tx
                    .send(SseEvent::ApprovalUpdated {
                        id: id.clone(),
                        status: ApprovalStatus::Approved,
                    })
                    .ok();
                serde_json::to_string(&*approvals).ok()
            }
            "deny" => {
                let reason = req.reason.as_deref().unwrap_or("denied").to_owned();
                approvals
                    .deny(&id, &reason)
                    .map_err(|e| ApiError::conflict(e.to_string()))?;
                live.agent
                    .events_tx
                    .send(SseEvent::ApprovalUpdated {
                        id: id.clone(),
                        status: ApprovalStatus::Denied,
                    })
                    .ok();
                serde_json::to_string(&*approvals).ok()
            }
            _ => {
                return Err(ApiError::bad_request(
                    "decision must be 'approve' or 'deny'",
                ));
            }
        };

        // Persist while still holding the lock so the agent loop's
        // concurrent persist() cannot interleave a stale write.
        if let (Some(json), Some(dir)) = (json, live.identity.agent_dir.as_ref())
            && let Err(e) = persistence::persist_approvals(&json, dir)
        {
            tracing::error!("approval persist after decision failed: {e:#}");
        }

        // Wake the agent so it can drain the notification and act on the
        // approval/denial.  The approval lock is still held here but will be
        // dropped on return; the agent task will briefly contend on the lock
        // and then proceed.
        live.agent.notify.notify_one();

        return Ok(axum::http::StatusCode::OK);
    }

    Err(ApiError::not_found("approval not found"))
}

#[cfg(test)]
mod tests {
    use axum::Json;
    use axum::extract::{Path, State};
    use kallip_common::agentid::AgentId;
    use kallip_common::policy::{ExecDecision, ExecOverride, ExecPolicy, PolicyPreset};
    use kallip_common::protocol::ApprovalDecisionBody;
    use kallip_shell::tools::names;

    use crate::auth::{AuthIdentity, Identity};
    use crate::test_helpers::*;

    // Helper: the JSON arguments for a deferred `bash_exec` of `command`.
    fn bash_args(command: &str) -> String {
        serde_json::json!({ "command": command }).to_string()
    }

    // -- Approval classify gate: respond_approval --

    #[tokio::test]
    async fn approval_gate_allows_when_superior_classify_allows() {
        // `ls` is read-only catalog → Allow under the default preset. A superior
        // may approve the deferred bash_exec.
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(
                &mut reg,
                &parent,
                PolicyPreset::Default,
                ExecPolicy::default(),
            );
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id = enqueue_committed_approval(
            &state.registry.read().await,
            &child,
            names::BASH_EXEC,
            &bash_args("ls"),
        )
        .await;

        let result = super::respond_approval(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: parent }),
            Path(approval_id),
            Json(ApprovalDecisionBody {
                decision: "approve".into(),
                reason: None,
            }),
        )
        .await;

        assert_eq!(result.unwrap(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn approval_gate_rejects_when_superior_classify_asks() {
        // `cargo` is absent from the catalog → Ask under default. The superior's
        // own rule-set would gate it, so approving the delegation is forbidden.
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(
                &mut reg,
                &parent,
                PolicyPreset::Default,
                ExecPolicy::default(),
            );
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id = enqueue_committed_approval(
            &state.registry.read().await,
            &child,
            names::BASH_EXEC,
            &bash_args("cargo build"),
        )
        .await;

        let result = super::respond_approval(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: parent }),
            Path(approval_id),
            Json(ApprovalDecisionBody {
                decision: "approve".into(),
                reason: None,
            }),
        )
        .await;

        match result {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected FORBIDDEN when superior classify asks"),
        }
    }

    #[tokio::test]
    async fn approval_gate_rejects_when_denylisted() {
        // `sed` is builtin-denied even under auto; approving it is forbidden.
        let state = make_state_with_preset(PolicyPreset::Auto);
        let parent = AgentId::random();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(&mut reg, &parent, PolicyPreset::Auto, ExecPolicy::default());
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id = enqueue_committed_approval(
            &state.registry.read().await,
            &child,
            names::BASH_EXEC,
            &bash_args("sed 's/a/b/' f"),
        )
        .await;

        let result = super::respond_approval(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: parent }),
            Path(approval_id),
            Json(ApprovalDecisionBody {
                decision: "approve".into(),
                reason: None,
            }),
        )
        .await;

        match result {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected FORBIDDEN for denylisted command"),
        }
    }

    #[tokio::test]
    async fn approval_gate_superior_exec_override_can_authorize() {
        // The superior widens `cargo` to Allow via exec-policy; under the default
        // preset that makes classify Allow, so the delegation is permitted. This
        // is the "approval gate is live" path: a wider parent override authorizes
        // a command the child deferred.
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        let mut exec = ExecPolicy::default();
        exec.overrides
            .insert("cargo".into(), ExecOverride::new(ExecDecision::Allow));

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(&mut reg, &parent, PolicyPreset::Default, exec);
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id = enqueue_committed_approval(
            &state.registry.read().await,
            &child,
            names::BASH_EXEC,
            &bash_args("cargo build"),
        )
        .await;

        let result = super::respond_approval(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: parent }),
            Path(approval_id),
            Json(ApprovalDecisionBody {
                decision: "approve".into(),
                reason: None,
            }),
        )
        .await;

        assert_eq!(result.unwrap(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn approval_gate_classifies_actual_command_not_any_allow() {
        // The superior allows `cargo` but the deferred command is `rm` (absent,
        // Ask under default). The gate must classify the actual command, so a
        // sibling override does not smuggle it through.
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        let mut exec = ExecPolicy::default();
        exec.overrides
            .insert("cargo".into(), ExecOverride::new(ExecDecision::Allow));

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(&mut reg, &parent, PolicyPreset::Default, exec);
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id = enqueue_committed_approval(
            &state.registry.read().await,
            &child,
            names::BASH_EXEC,
            &bash_args("rm -rf /tmp/x"),
        )
        .await;

        let result = super::respond_approval(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: parent }),
            Path(approval_id),
            Json(ApprovalDecisionBody {
                decision: "approve".into(),
                reason: None,
            }),
        )
        .await;

        match result {
            Err(e) => assert_eq!(e.status, 403),
            Ok(_) => panic!("expected FORBIDDEN: gate must classify the actual command"),
        }
    }

    #[tokio::test]
    async fn approval_gate_operator_exempt() {
        // The operator may approve a command that an agent superior could not.
        let state = make_state();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &child);
        }

        let approval_id = enqueue_committed_approval(
            &state.registry.read().await,
            &child,
            names::BASH_EXEC,
            &bash_args("cargo build"),
        )
        .await;

        let result = super::respond_approval(
            State(state),
            AuthIdentity::test_new(Identity::Operator),
            Path(approval_id),
            Json(ApprovalDecisionBody {
                decision: "approve".into(),
                reason: None,
            }),
        )
        .await;

        assert_eq!(result.unwrap(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn approval_deny_always_allowed() {
        // Any superior may deny, regardless of the classify verdict.
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(
                &mut reg,
                &parent,
                PolicyPreset::Default,
                ExecPolicy::default(),
            );
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id = enqueue_committed_approval(
            &state.registry.read().await,
            &child,
            names::BASH_EXEC,
            &bash_args("sed 's/a/b/' f"),
        )
        .await;

        let result = super::respond_approval(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: parent }),
            Path(approval_id),
            Json(ApprovalDecisionBody {
                decision: "deny".into(),
                reason: Some("test deny".into()),
            }),
        )
        .await;

        assert_eq!(result.unwrap(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn approval_gate_skips_non_bash_deferred_action() {
        // Only bash_exec has a classify gate. A deferred non-bash action needs no
        // gate (there is no security surface), so a superior approves it cleanly.
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(
                &mut reg,
                &parent,
                PolicyPreset::Default,
                ExecPolicy::default(),
            );
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id = enqueue_committed_approval(
            &state.registry.read().await,
            &child,
            "some_self_management_tool",
            "{}",
        )
        .await;

        let result = super::respond_approval(
            State(state),
            AuthIdentity::test_new(Identity::Agent { id: parent }),
            Path(approval_id),
            Json(ApprovalDecisionBody {
                decision: "approve".into(),
                reason: None,
            }),
        )
        .await;

        assert_eq!(result.unwrap(), axum::http::StatusCode::OK);
    }
}
