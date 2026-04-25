use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::context::AgenticContext;

/// Reports the agent's current token budget and usage.
pub struct ContextStatusTool {
    ctx: Arc<Mutex<dyn AgenticContext>>,
}

impl ContextStatusTool {
    pub fn new(ctx: Arc<Mutex<dyn AgenticContext>>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl LlmTool for ContextStatusTool {
    fn name(&self) -> &str {
        "context_status"
    }

    fn description(&self) -> &str {
        "Report the agent's current context window usage: how many tokens are \
         consumed by pinned items, summary, and conversation turns, and how many \
         remain. Use this to decide whether to evict old turns with context_evict \
         before the automatic compaction triggers."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn call(&self, _args_json: &str) -> Result<String> {
        let ctx = self.ctx.lock().await;
        let usage = ctx.usage_snapshot();
        let pinned_tokens: usize = usage.pinned_items.iter().map(|(_, t)| *t).sum();
        Ok(serde_json::to_string(&json!({
            "last_prompt_tokens": usage.last_prompt_tokens,
            "usage": {
                "pinned_tokens": pinned_tokens,
                "summary_tokens": usage.summary_tokens,
                "turn_tokens": usage.turn_tokens,
            },
            "pinned_items": usage.pinned_items,
            "turn_count": usage.turn_count,
            "has_summary": usage.summary_tokens > 0,
        }))?)
    }
}
