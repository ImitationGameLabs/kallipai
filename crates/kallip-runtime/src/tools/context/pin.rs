use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use just_llm_client::types::chat::ChatMessage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::context::AgenticContext;

#[derive(Debug, Deserialize, Serialize)]
struct PinArgs {
    label: String,
    content: String,
}

/// Tool that injects caller-provided content into the agent's persistent context.
///
/// `context_pin` is **content-based**: the agent supplies the full text to persist, which is
/// stored verbatim as a new persistent entry prepended to the context. It does *not* reference
/// an existing conversation turn.
///
/// Re-stating important content to pin it is intentional — it doubles as attention
/// reinforcement for a generative agent. By-reference pinning was considered and rejected: it
/// would need fragile turn addressing prone to off-by-one and stale-index errors.
pub struct ContextPinTool {
    ctx: Arc<Mutex<dyn AgenticContext>>,
}

impl ContextPinTool {
    /// Tool name exposed to the LLM and referenced by the policy layer.
    pub const NAME: &str = "context_pin";

    pub fn new(ctx: Arc<Mutex<dyn AgenticContext>>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl LlmTool for ContextPinTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Pin content into the agent's persistent context. Pinned content is \
         included in every LLM request until explicitly removed with context_unpin. \
         Use this to keep important instructions, constraints, or reference material \
         available. Provide the complete content to persist — it is stored verbatim, \
         not as a reference to earlier messages."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Unique identifier for this pinned item."
                },
                "content": {
                    "type": "string",
                    "description": "The content to pin."
                }
            },
            "required": ["label", "content"]
        })
    }

    async fn call(&self, args_json: &str) -> Result<String> {
        let args: PinArgs =
            serde_json::from_str(args_json).context("context_pin: invalid arguments")?;
        let mut ctx = self.ctx.lock().await;
        ctx.pin(&args.label, ChatMessage::user(&args.content))?;
        let labels = ctx.pinned_labels();
        Ok(serde_json::to_string(&json!({
            "pinned": args.label,
            "pinned_labels": labels,
        }))?)
    }
}
