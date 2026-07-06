use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::context::AgenticContext;
use just_llm_client::types::chat::ChatMessage;

#[derive(Debug, Deserialize, Serialize)]
struct EvictArgs {
    /// Summary text preserving key facts from the evicted turns.
    summary: String,
}

/// Evicts all conversation turns and replaces them with a summary.
///
/// The summary is pinned as `context_summary`, overwriting any existing one.
/// This is the only supported eviction path: you must preserve what matters.
pub struct ContextEvictTool {
    ctx: Arc<Mutex<dyn AgenticContext>>,
}

impl ContextEvictTool {
    /// Tool name exposed to the LLM and referenced by the policy layer.
    pub const NAME: &str = "context_evict";

    pub fn new(ctx: Arc<Mutex<dyn AgenticContext>>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl LlmTool for ContextEvictTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Evict all conversation turns, replacing them with a summary you provide. \
         The summary is pinned as context_summary — write it to preserve the key \
         facts, decisions, and current state that you need from the turns being \
         discarded. Use context_status to review current turns before writing \
         your summary."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Summary preserving key facts from the conversation turns being evicted."
                }
            },
            "required": ["summary"]
        })
    }

    async fn call(&self, args_json: &str) -> Result<String> {
        let args: EvictArgs =
            serde_json::from_str(args_json).context("context_evict: invalid arguments")?;

        let mut ctx = self.ctx.lock().await;
        let turn_count = ctx.usage_snapshot().turn_count;
        if turn_count == 0 {
            return Ok(serde_json::to_string(&json!({
                "evicted": 0,
                "remaining_turns": 0,
                "freed_tokens": 0,
            }))?);
        }

        ctx.replace_pin("context_summary", ChatMessage::assistant(&args.summary))?;
        let result = ctx.evict_turns(turn_count);
        ctx.reset_context_warnings();

        Ok(serde_json::to_string(&json!({
            "evicted": result.evicted,
            "remaining_turns": result.remaining_turns,
            "freed_tokens": result.freed_tokens,
        }))?)
    }
}
