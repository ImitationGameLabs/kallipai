use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use just_llm_client::ToolDispatcher;
use just_llm_client::types::chat::{FunctionDefinition, ToolDefinition, ToolType};
use serde_json::json;
use shell::{PtyBuilder, shell_tool_set};
use tokio::sync::Mutex;

use crate::context::{AgenticContext, ContextStore};
pub mod context;
pub mod shell;
pub mod skill;

pub use skill::{ensure_meta_skill, load_skill};

/// Builds the tool registry exposed by `just-agent`.
///
/// Spawns bash via [`PtyBuilder`], preserving full shell session state.
/// The shell's working directory is the process current directory (set by
/// the caller via `std::env::set_current_dir`).
///
/// Context tools share the same `ContextStore` as the main loop.
pub async fn build_tool_dispatch(
    ctx: Arc<Mutex<ContextStore>>,
    env: HashMap<String, String>,
) -> Result<ToolDispatcher> {
    let backend = PtyBuilder::new("main").envs(env).build().await?;
    let backend = Arc::new(Mutex::new(backend));

    let mut dispatch = ToolDispatcher::new();
    dispatch.add_tools(shell_tool_set(backend))?;
    let ctx_dyn: Arc<Mutex<dyn AgenticContext>> = ctx;
    dispatch.add_tools(context::context_tool_set(ctx_dyn.clone()))?;
    dispatch.add_tools(skill::skill_tool_set(ctx_dyn))?;

    Ok(dispatch)
}

// ---------------------------------------------------------------------------
// Approval meta-tool definitions (handled by executor, not dispatcher)
// ---------------------------------------------------------------------------

pub fn approval_list_definition() -> ToolDefinition {
    ToolDefinition {
        kind: ToolType::Function,
        function: FunctionDefinition {
            name: "approval_list".into(),
            description: Some(
                "List deferred tool actions awaiting or having received approval. \
                 Filter by status: pending, approved, denied, redeemed, cancelled. \
                 Returns action details including request_id needed for redeem/cancel."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["pending", "approved", "denied", "redeemed", "cancelled", "all"],
                        "description": "Filter by status. Omit to list all."
                    }
                }
            })),
            strict: None,
        },
    }
}

pub fn approval_redeem_definition() -> ToolDefinition {
    ToolDefinition {
        kind: ToolType::Function,
        function: FunctionDefinition {
            name: "approval_redeem".into(),
            description: Some(
                "Execute a previously deferred tool action that has been approved. \
                 The stored tool call runs and returns its result. \
                 Only works on actions with 'approved' status."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "request_id": {
                        "type": "string",
                        "description": "The request_id from the deferred action."
                    }
                },
                "required": ["request_id"]
            })),
            strict: None,
        },
    }
}

pub fn approval_cancel_definition() -> ToolDefinition {
    ToolDefinition {
        kind: ToolType::Function,
        function: FunctionDefinition {
            name: "approval_cancel".into(),
            description: Some(
                "Cancel a pending deferred action that is no longer needed. \
                 Only works on actions with 'pending' status."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "request_id": {
                        "type": "string",
                        "description": "The request_id to cancel."
                    }
                },
                "required": ["request_id"]
            })),
            strict: None,
        },
    }
}

/// Generate a human-readable summary for a deferred tool call.
pub fn make_summary(tool_name: &str, args_json: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json)
        && tool_name == "shell_session_exec"
        && let Some(cmd) = v.get("command").and_then(|v| v.as_str())
    {
        return truncate(cmd, 120);
    }
    let truncated = truncate(args_json, 80);
    format!("{tool_name}({truncated})")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_owned() } else { format!("{}...", &s[..max]) }
}
