//! Authorized tool executor with deferred approval.

use std::sync::Arc;

use anyhow::Result;
use just_llm_client::{ToolDispatcher, types::chat::ToolDefinition};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::AgentPolicy;
use crate::deferred::{DeferredQueue, DeferredStatus, deferred_result_json};
use crate::tools;

/// Executes tools behind a policy gate with deferred approval.
///
/// When policy returns `Ask`, the tool call is stored in the
/// [`DeferredQueue`] and a deferred result is returned immediately.
/// The LLM can continue working. When approval arrives, the LLM
/// calls `approval_redeem` to execute the stored action.
pub struct AuthorizedToolExecutor {
    dispatch: ToolDispatcher,
    policy: AgentPolicy,
    deferred: Arc<Mutex<DeferredQueue>>,
}

impl AuthorizedToolExecutor {
    pub fn new(
        dispatch: ToolDispatcher,
        policy: AgentPolicy,
        deferred: Arc<Mutex<DeferredQueue>>,
    ) -> Self {
        Self { dispatch, policy, deferred }
    }

    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = self.dispatch.tool_definitions();
        defs.push(tools::approval_list_definition());
        defs.push(tools::approval_redeem_definition());
        defs.push(tools::approval_cancel_definition());
        defs
    }

    pub async fn execute(&mut self, tool_name: &str, args_json: &str) -> String {
        match tool_name {
            "approval_list" => self.handle_list(args_json).await,
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
            super::ToolDecision::Ask { reason, dangerous } => {
                let summary = tools::make_summary(tool_name, args_json);
                let mut q = self.deferred.lock().await;
                let id = q.enqueue(tool_name, args_json, &summary, &reason, dangerous);
                deferred_result_json(&id, tool_name, &reason, dangerous)
            }
        }
    }

    async fn handle_list(&self, args_json: &str) -> String {
        let status_filter = parse_status_filter(args_json);
        let q = self.deferred.lock().await;
        let items: Vec<Value> = match status_filter {
            Some(ref s) => q.list(Some(s)),
            None => q.list(None),
        }
        .into_iter()
        .map(|a| {
            json!({
                "request_id": a.request_id,
                "tool_name": a.tool_name,
                "summary": a.summary,
                "reason": a.reason,
                "dangerous": a.dangerous,
                "status": a.status,
            })
        })
        .collect();
        json!({"ok": true, "actions": items}).to_string()
    }

    async fn handle_redeem(&mut self, args_json: &str) -> String {
        let request_id = match parse_request_id(args_json) {
            Ok(id) => id,
            Err(e) => return error_result("approval_redeem", e.to_string()),
        };
        let action = {
            let mut q = self.deferred.lock().await;
            match q.take_for_redeem(&request_id) {
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
        let request_id = match parse_request_id(args_json) {
            Ok(id) => id,
            Err(e) => return error_result("approval_cancel", e.to_string()),
        };
        let mut q = self.deferred.lock().await;
        match q.cancel(&request_id) {
            Ok(()) => json!({"ok": true, "cancelled": request_id}).to_string(),
            Err(e) => error_result("approval_cancel", e.to_string()),
        }
    }
}

fn parse_request_id(args_json: &str) -> Result<String> {
    let v: Value = serde_json::from_str(args_json)?;
    v.get("request_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid 'request_id' field"))
}

fn parse_status_filter(args_json: &str) -> Option<DeferredStatus> {
    let v: Value = serde_json::from_str(args_json).ok()?;
    let s = v.get("status")?.as_str()?;
    match s {
        "pending" => Some(DeferredStatus::Pending),
        "approved" => Some(DeferredStatus::Approved),
        "denied" => Some(DeferredStatus::Denied { reason: String::new() }),
        "redeemed" => Some(DeferredStatus::Redeemed),
        "cancelled" => Some(DeferredStatus::Cancelled),
        _ => None,
    }
}

fn success_result(tool_name: &str, output: String) -> String {
    let parsed = serde_json::from_str::<Value>(&output).unwrap_or(Value::String(output));
    json!({
        "ok": true,
        "tool_name": tool_name,
        "result": parsed,
    })
    .to_string()
}

fn error_result(tool_name: &str, error: String) -> String {
    json!({
        "ok": false,
        "tool_name": tool_name,
        "error": error,
    })
    .to_string()
}
