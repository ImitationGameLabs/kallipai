use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use just_agent_common::policy::ExecPolicy;
use just_agent_shell::{StatelessBuilder, bash_exec_tool_set};
use just_llm_client::ToolDispatcher;
use just_llm_client::types::chat::{FunctionDefinition, ToolDefinition, ToolType};
use serde_json::json;
use tokio::sync::Mutex;

use crate::context::{AgenticContext, ContextStore};
pub mod context;
pub mod skill;

pub use skill::{
    META_SKILL_NAME, load_skill, meta_skill_content, parse_frontmatter_meta, skill_dir,
    skill_metadata, validate_skill_name,
};

/// Builds the tool registry exposed by `just-agent`.
///
/// Spawns a fresh isolated `bash` per command via [`StatelessBuilder`] (the stateless
/// one-shot backend). The working directory is read fresh from `pwd` after each command and
/// reported in the tool result — it does not persist implicitly across calls. A background task
/// that finishes delivers a completion notice through `notice_sink` (the daemon wires it to the agent's
/// prompt channel, so the LLM learns without polling `bash_background_read`).
///
/// Context tools share the same `ContextStore` as the main loop.
pub async fn build_tool_dispatch(
    ctx: Arc<Mutex<ContextStore>>,
    env: HashMap<String, String>,
    notice_sink: Arc<dyn Fn(String) + Send + Sync>,
    exec_policy: Arc<RwLock<ExecPolicy>>,
) -> Result<ToolDispatcher> {
    let backend = StatelessBuilder::new()
        .envs(env)
        // The exit code is intentionally omitted from the notice — the agent reads it
        // (and the output) via `bash_background_read`. Keeping the notice minimal avoids
        // duplicating state the agent will fetch anyway.
        .on_terminal(move |id, state, _code| {
            notice_sink(format!("[Background task {id} {}]", state.as_str()));
        })
        .build()
        .await?;
    let backend = Arc::new(Mutex::new(backend));

    let mut dispatch = ToolDispatcher::new();
    dispatch.add_tools(bash_exec_tool_set(backend))?;
    let ctx_dyn: Arc<Mutex<dyn AgenticContext>> = ctx;
    dispatch.add_tools(context::context_tool_set(ctx_dyn.clone(), exec_policy))?;
    dispatch.add_tools(skill::file_pin_tool_set(ctx_dyn))?;

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
                "List approval requests awaiting or having received a decision. \
                 Filter by status: pending, committed, approved, denied, redeemed, cancelled. \
                 Returns approval details including id needed for commit/redeem/cancel."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["pending", "committed", "approved", "denied", "redeemed", "cancelled", "all"],
                        "description": "Filter by status. Omit to list all."
                    }
                }
            })),
            strict: None,
        },
    }
}

pub fn approval_commit_definition() -> ToolDefinition {
    ToolDefinition {
        kind: ToolType::Function,
        function: FunctionDefinition {
            name: "approval_commit".into(),
            description: Some(
                "Submit an approval request with your justification for \
                 why this tool call is necessary. After committing, the request becomes \
                 visible to an approver. Only works on approvals with 'pending' status."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The id of the approval to commit."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Your justification for why this tool call is necessary."
                    }
                },
                "required": ["id", "reason"]
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
                "Execute a previously approved tool action. \
                 The stored tool call runs and returns its result. \
                 Only works on approvals with 'approved' status."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The id of the approval to redeem."
                    }
                },
                "required": ["id"]
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
                "Cancel an approval that is no longer needed. \
                 Works on pending, committed, approved, and denied approvals."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The id of the approval to cancel."
                    }
                },
                "required": ["id"]
            })),
            strict: None,
        },
    }
}
