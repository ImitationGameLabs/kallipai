use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::context::AgenticContext;

#[derive(Debug, Deserialize, Serialize)]
struct EvictArgs {
    count: usize,
}

/// Evicts the oldest conversation turns to free context capacity.
pub struct ContextEvictTool {
    ctx: Arc<Mutex<dyn AgenticContext>>,
}

impl ContextEvictTool {
    pub fn new(ctx: Arc<Mutex<dyn AgenticContext>>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl LlmTool for ContextEvictTool {
    fn name(&self) -> &str {
        "context_evict"
    }

    fn description(&self) -> &str {
        "Evict the oldest conversation turns to free context capacity. Evicted turns \
         are permanently discarded (not summarized). Pin important content with \
         context_pin before evicting if you need to preserve it. Use context_status \
         to check current usage before deciding how many turns to evict."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "count": {
                    "type": "integer",
                    "description": "Number of oldest turns to evict."
                }
            },
            "required": ["count"]
        })
    }

    async fn call(&self, args_json: &str) -> Result<String> {
        let args: EvictArgs =
            serde_json::from_str(args_json).context("context_evict: invalid arguments")?;
        let mut ctx = self.ctx.lock().await;
        let result = ctx.evict_turns(args.count);
        Ok(serde_json::to_string(&json!({
            "evicted": result.evicted,
            "remaining_turns": result.remaining_turns,
            "freed_tokens": result.freed_tokens,
        }))?)
    }
}
