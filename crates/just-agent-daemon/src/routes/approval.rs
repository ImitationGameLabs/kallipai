use axum::Json;
use axum::extract::{Path, Query, State};
use just_agent_common::approval::ApprovalStatus;
use just_agent_common::policy::PolicyDecision;
use just_agent_common::protocol::{
    ApiError, ApprovalDecisionBody, ApprovalEntry, ListApprovalsResponse, SseEvent,
};
use just_agent_runtime::persistence;

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
        let q = entry.agent.approvals.lock().await;
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
        let approvals = entry.agent.approvals.lock().await;
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
/// For **approve** decisions, an additional policy gate applies: the caller's
/// own [`ToolPolicy`](just_agent_common::policy::ToolPolicy) must permit the tool with `PolicyDecision::Allow`.
/// This prevents a superior from using subordinates as proxies to bypass its
/// own tool restrictions. The operator identity is exempt from this check.
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
        let mut approvals = entry.agent.approvals.lock().await;
        if !approvals.contains(&id) {
            continue;
        }

        registry.require_superior(auth.identity(), agent_id)?;

        let info = approvals.get(&id).expect("contains checked above");

        let json = match req.decision.as_str() {
            "approve" => {
                // Policy gate: a superior may only approve if its own policy
                // allows the tool unilaterally. Operator is exempt.
                if let crate::auth::Identity::Agent { id: caller_id } = auth.identity() {
                    let caller_entry = registry
                        .get(caller_id)
                        .ok_or_else(|| ApiError::internal("caller agent not found in registry"))?;
                    let caller_decision = caller_entry
                        .agent
                        .tool_policy
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .decision_for(&info.content.tool_name);
                    if caller_decision != PolicyDecision::Allow {
                        return Err(ApiError::forbidden(format!(
                            "cannot approve '{}': caller policy is '{caller_decision}' \
                             (only 'allow' permits unilateral delegation)",
                            info.content.tool_name,
                        )));
                    }
                }

                approvals
                    .approve(&id)
                    .map_err(|e| ApiError::conflict(e.to_string()))?;
                entry
                    .agent
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
                entry
                    .agent
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
        if let (Some(json), Some(dir)) = (json, entry.agent.agent_dir.as_ref())
            && let Err(e) = persistence::persist_approvals(&json, dir)
        {
            tracing::error!("approval persist after decision failed: {e:#}");
        }

        // Wake the agent so it can drain the notification and act on the
        // approval/denial.  The approval lock is still held here but will be
        // dropped on return; the agent task will briefly contend on the lock
        // and then proceed.
        entry.agent.notify.notify_one();

        return Ok(axum::http::StatusCode::OK);
    }

    Err(ApiError::not_found("approval not found"))
}

#[cfg(test)]
mod tests {
    use axum::Json;
    use axum::extract::{Path, State};
    use just_agent_common::agentid::AgentId;
    use just_agent_common::policy::{PolicyDecision, ToolPolicy};
    use just_agent_common::protocol::ApprovalDecisionBody;
    use std::collections::BTreeMap;

    use crate::auth::{AuthIdentity, Identity};
    use crate::test_helpers::*;

    // -- Approval policy gate: respond_approval --

    #[tokio::test]
    async fn approval_policy_gate_allows_when_superior_has_allow() {
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(&mut reg, &parent, policy_allow_tool("dangerous_tool"));
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id =
            enqueue_committed_approval(&state.registry.read().await, &child, "dangerous_tool")
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
    async fn approval_policy_gate_rejects_when_superior_has_ask() {
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(
                &mut reg,
                &parent,
                policy_for_tool("dangerous_tool", PolicyDecision::Ask),
            );
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id =
            enqueue_committed_approval(&state.registry.read().await, &child, "dangerous_tool")
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
            Err(e) => {
                assert_eq!(e.status, 403);
                assert!(e.message.contains("dangerous_tool"));
                assert!(e.message.contains("ask"));
            }
            Ok(_) => panic!("expected FORBIDDEN for superior with Ask policy"),
        }
    }

    #[tokio::test]
    async fn approval_policy_gate_rejects_when_superior_has_deny() {
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(
                &mut reg,
                &parent,
                policy_for_tool("dangerous_tool", PolicyDecision::Deny),
            );
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id =
            enqueue_committed_approval(&state.registry.read().await, &child, "dangerous_tool")
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
            Err(e) => {
                assert_eq!(e.status, 403);
                assert!(e.message.contains("deny"));
            }
            Ok(_) => panic!("expected FORBIDDEN for superior with Deny policy"),
        }
    }

    #[tokio::test]
    async fn approval_policy_gate_rejects_when_superior_has_classify() {
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(
                &mut reg,
                &parent,
                policy_for_tool("dangerous_tool", PolicyDecision::Classify),
            );
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id =
            enqueue_committed_approval(&state.registry.read().await, &child, "dangerous_tool")
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
            Err(e) => {
                assert_eq!(e.status, 403);
                assert!(e.message.contains("classify"));
            }
            Ok(_) => panic!("expected FORBIDDEN for superior with Classify policy"),
        }
    }

    #[tokio::test]
    async fn approval_policy_gate_operator_exempt() {
        let state = make_state();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root(&mut reg, &child);
        }

        let approval_id =
            enqueue_committed_approval(&state.registry.read().await, &child, "dangerous_tool")
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
    async fn approval_deny_always_allowed_regardless_of_policy() {
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(
                &mut reg,
                &parent,
                policy_for_tool("dangerous_tool", PolicyDecision::Deny),
            );
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id =
            enqueue_committed_approval(&state.registry.read().await, &child, "dangerous_tool")
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
    async fn approval_policy_gate_checks_specific_tool_not_any_allow() {
        let state = make_state();
        let parent = AgentId::random();
        let child = AgentId::random();

        let mut tools = BTreeMap::new();
        tools.insert("safe_tool".to_string(), PolicyDecision::Allow);
        tools.insert("dangerous_tool".to_string(), PolicyDecision::Ask);
        let policy = ToolPolicy {
            default: PolicyDecision::Ask,
            tools,
        };

        {
            let mut reg = state.registry.write().await;
            add_root_with_policy(&mut reg, &parent, policy);
            add_sub(&mut reg, &child, &parent);
        }

        let approval_id =
            enqueue_committed_approval(&state.registry.read().await, &child, "dangerous_tool")
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
            Err(e) => {
                assert_eq!(e.status, 403);
                assert!(e.message.contains("dangerous_tool"));
                assert!(e.message.contains("ask"));
            }
            Ok(_) => {
                panic!("expected FORBIDDEN — gate must check the specific tool, not any Allow")
            }
        }
    }
}
