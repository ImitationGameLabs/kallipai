use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::context::AgenticContext;

#[derive(Debug, Deserialize, Serialize)]
struct UnpinArgs {
    label: String,
}

/// Removes a pinned item by label.
pub struct ContextUnpinTool {
    ctx: Arc<Mutex<dyn AgenticContext>>,
}

impl ContextUnpinTool {
    /// Tool name exposed to the LLM and referenced by the policy layer.
    pub const NAME: &str = "context_unpin";

    pub fn new(ctx: Arc<Mutex<dyn AgenticContext>>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl LlmTool for ContextUnpinTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Remove a pinned item from the agent's context by label. \
         The content will no longer be included in future LLM requests."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "The label of the pinned item to remove."
                }
            },
            "required": ["label"]
        })
    }

    async fn call(&self, args_json: &str) -> Result<String> {
        let args: UnpinArgs =
            serde_json::from_str(args_json).context("context_unpin: invalid arguments")?;
        let mut ctx = self.ctx.lock().await;
        ctx.unpin(&args.label)?;
        let labels = ctx.pinned_labels();
        Ok(serde_json::to_string(&json!({
            "unpinned": args.label,
            "pinned_labels": labels,
        }))?)
    }
}
