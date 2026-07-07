//! Authorized tool executor with approval gating.

use std::sync::Arc;

use anyhow::Result;
use just_llm_client::{ToolDispatcher, types::chat::ToolDefinition};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

use super::AgentPolicy;
use crate::approval::{ApprovalStatus, ApprovalStore, approval_result_json};
use crate::tools;

/// Executes tools behind a policy gate with approval gating.
///
/// When policy returns `Ask`, the tool call is stored in the
/// [`ApprovalStore`] and a pending-approval result is returned immediately.
/// The LLM can continue working. When approval arrives, the LLM
/// calls `approval_redeem` to execute the stored action.
pub struct AuthorizedToolExecutor {
    dispatch: ToolDispatcher,
    policy: AgentPolicy,
    approvals: Arc<Mutex<ApprovalStore>>,
}

impl AuthorizedToolExecutor {
    pub fn new(
        dispatch: ToolDispatcher,
        policy: AgentPolicy,
        approvals: Arc<Mutex<ApprovalStore>>,
    ) -> Self {
        Self {
            dispatch,
            policy,
            approvals,
        }
    }

    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = self.dispatch.tool_definitions();
        defs.push(tools::approval_list_definition());
        defs.push(tools::approval_commit_definition());
        defs.push(tools::approval_redeem_definition());
        defs.push(tools::approval_cancel_definition());
        defs
    }

    pub async fn execute(&mut self, tool_name: &str, args_json: &str) -> String {
        match tool_name {
            "approval_list" => self.handle_list(args_json).await,
            "approval_commit" => self.handle_commit(args_json).await,
            "approval_redeem" => self.handle_redeem(args_json).await,
            "approval_cancel" => self.handle_cancel(args_json).await,
            _ => self.execute_tool(tool_name, args_json).await,
        }
    }

    async fn execute_tool(&mut self, tool_name: &str, args_json: &str) -> String {
        let decision = match self.policy.evaluate(tool_name, args_json) {
            Ok(d) => d,
            Err(e) => return error_result(tool_name, format!("policy evaluation failed: {e:#}")),
        };

        match decision {
            super::ToolDecision::Allow => match self.dispatch.call_tool(tool_name, args_json).await
            {
                Ok(output) => success_result(tool_name, output),
                Err(e) => error_result(tool_name, e.to_string()),
            },
            super::ToolDecision::Deny { reason } => {
                error_result(tool_name, format!("tool denied: {reason}"))
            }
            super::ToolDecision::Ask { reason } => {
                let mut q = self.approvals.lock().await;
                let id = q.enqueue(tool_name, args_json, reason.clone());
                approval_result_json(&id, tool_name, reason.as_deref())
            }
        }
    }

    async fn handle_list(&self, args_json: &str) -> String {
        let status_filter = parse_status_filter(args_json);
        let q = self.approvals.lock().await;
        let items: Vec<_> = q
            .list(status_filter.as_ref())
            .into_iter()
            .map(|a| ApprovalListItem {
                id: a.id,
                content: a.content,
                commit_reason: a.commit_reason,
                status: a.status,
                deny_reason: a.deny_reason,
                defer_reason: a.defer_reason,
                created_at: a.created_at,
            })
            .collect();
        serde_json::to_string(&ApprovalListResponse {
            ok: true,
            actions: items,
        })
        .unwrap_or_else(|e| error_result("approval_list", e.to_string()))
    }

    async fn handle_commit(&self, args_json: &str) -> String {
        let (id, reason) = match parse_commit_args(args_json) {
            Ok(v) => v,
            Err(e) => return error_result("approval_commit", e.to_string()),
        };
        let mut q = self.approvals.lock().await;
        match q.commit(&id, &reason) {
            Ok(()) => serde_json::to_string(&ApprovalCommitResponse {
                ok: true,
                committed: true,
                id,
            })
            .unwrap_or_else(|e| error_result("approval_commit", e.to_string())),
            Err(e) => error_result("approval_commit", e.to_string()),
        }
    }

    async fn handle_redeem(&mut self, args_json: &str) -> String {
        let id = match parse_id(args_json) {
            Ok(id) => id,
            Err(e) => return error_result("approval_redeem", e.to_string()),
        };
        let action = {
            let mut q = self.approvals.lock().await;
            match q.take_for_redeem(&id) {
                Ok(a) => a,
                Err(e) => return error_result("approval_redeem", e.to_string()),
            }
        };
        match self
            .dispatch
            .call_tool(&action.tool_name, &action.args_json)
            .await
        {
            Ok(output) => success_result(&action.tool_name, output),
            Err(e) => error_result(&action.tool_name, e.to_string()),
        }
    }

    async fn handle_cancel(&self, args_json: &str) -> String {
        let id = match parse_id(args_json) {
            Ok(id) => id,
            Err(e) => return error_result("approval_cancel", e.to_string()),
        };
        let mut q = self.approvals.lock().await;
        match q.cancel(&id) {
            Ok(prev) => serde_json::to_string(&ApprovalCancelResponse {
                ok: true,
                cancelled: id,
                previous_status: prev.to_string(),
            })
            .unwrap_or_else(|e| error_result("approval_cancel", e.to_string())),
            Err(e) => error_result("approval_cancel", e.to_string()),
        }
    }
}

fn parse_id(args_json: &str) -> Result<String> {
    let v: Value = serde_json::from_str(args_json)?;
    v.get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid 'id' field"))
}

fn parse_status_filter(args_json: &str) -> Option<ApprovalStatus> {
    let v: Value = serde_json::from_str(args_json).ok()?;
    let s = v.get("status")?.as_str()?;
    ApprovalStatus::from_str_name(s)
}

fn parse_commit_args(args_json: &str) -> Result<(String, String)> {
    let v: Value = serde_json::from_str(args_json)?;
    let id = v
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid 'id' field"))?
        .to_owned();
    let reason = v
        .get("reason")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid 'reason' field"))?
        .to_owned();
    Ok((id, reason))
}

fn success_result(tool_name: &str, output: String) -> String {
    let parsed = serde_json::from_str::<Value>(&output).unwrap_or(Value::String(output));
    serde_json::to_string(&ToolResultResponse {
        ok: true,
        tool_name: tool_name.to_owned(),
        result: parsed,
    })
    .unwrap_or_else(|e| error_result(tool_name, e.to_string()))
}

fn error_result(tool_name: &str, error: String) -> String {
    serde_json::to_string(&ToolErrorResponse {
        ok: false,
        tool_name: tool_name.to_owned(),
        error,
    })
    .unwrap_or_else(|_| r#"{"ok":false,"error":"serialization failed"}"#.to_owned())
}

// -- Typed response structs --

#[derive(Serialize)]
struct ToolResultResponse {
    ok: bool,
    tool_name: String,
    result: Value,
}

#[derive(Serialize)]
struct ToolErrorResponse {
    ok: bool,
    tool_name: String,
    error: String,
}

#[derive(Serialize)]
struct ApprovalListResponse {
    ok: bool,
    actions: Vec<ApprovalListItem>,
}

#[derive(Serialize)]
struct ApprovalListItem {
    id: String,
    content: kallip_common::approval::ToolCallContent,
    commit_reason: Option<String>,
    status: ApprovalStatus,
    deny_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    defer_reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: time::OffsetDateTime,
}

#[derive(Serialize)]
struct ApprovalCommitResponse {
    ok: bool,
    committed: bool,
    id: String,
}

#[derive(Serialize)]
struct ApprovalCancelResponse {
    ok: bool,
    cancelled: String,
    previous_status: String,
}
