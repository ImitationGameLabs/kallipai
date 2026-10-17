use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::context::AgenticContext;
use crate::context::{CONTEXT_SUMMARY_LABEL, summary_record};

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

        ctx.replace_pin(CONTEXT_SUMMARY_LABEL, summary_record(&args.summary))?;
        let result = ctx.evict_turns(turn_count);
        ctx.reset_context_warnings();

        Ok(serde_json::to_string(&json!({
            "evicted": result.evicted,
            "remaining_turns": result.remaining_turns,
            "freed_tokens": result.freed_tokens,
        }))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{
        CONTEXT_SUMMARY_HEADER, CONTEXT_SUMMARY_LABEL, ContextStore, compose_context,
    };
    use crate::test_support::{tool_call_msg, tool_result_msg, user_msg};

    async fn run(tool: &ContextEvictTool, args: Value) -> Result<String> {
        tool.call(&serde_json::to_string(&args).unwrap()).await
    }

    /// The pinned summary is a user record: our note about earlier turns, never
    /// a model turn.
    #[tokio::test]
    async fn evict_pins_the_summary_as_a_user_record() {
        let mut store = ContextStore::new();
        store.push_turn(vec![user_msg("earlier work")]);
        let store = Arc::new(Mutex::new(store));
        let tool = ContextEvictTool::new(store.clone());

        run(&tool, json!({ "summary": "we did the thing" }))
            .await
            .unwrap();

        let guard = store.lock().await;
        let pinned = guard
            .turns()
            .iter()
            .find(|t| t.is_pinned() && t.label() == Some(CONTEXT_SUMMARY_LABEL))
            .expect("the summary is pinned");
        let message = &pinned.messages[0];
        assert_eq!(message.role(), "user");
        assert_eq!(message.tool_call_id(), None);
        let text = message.content().unwrap_or_default();
        assert!(text.starts_with(CONTEXT_SUMMARY_HEADER));
        assert!(text.ends_with("we did the thing"));
    }

    /// The request that follows an eviction carries the pinned block and then
    /// the eviction round itself, and it must never carry two adjacent
    /// assistant messages (nor end on one).
    #[tokio::test]
    async fn request_after_eviction_never_pairs_assistant_messages() {
        let mut store = ContextStore::new();
        store.push_turn(vec![user_msg("earlier work")]);
        store.push_turn(vec![user_msg("more work")]);
        let store = Arc::new(Mutex::new(store));
        let tool = ContextEvictTool::new(store.clone());

        run(&tool, json!({ "summary": "we did the thing" }))
            .await
            .unwrap();

        // The round that called the tool is recorded after the drain.
        {
            let mut guard = store.lock().await;
            guard.push_turn(vec![
                tool_call_msg("c1", "context_evict", "{}"),
                tool_result_msg("{}", "c1"),
            ]);
        }

        let messages = compose_context(store.clone()).await;
        let paired = messages.windows(2).any(|pair| {
            pair[0].role() == "assistant"
                && pair[0].reasoning().is_none()
                && pair[1].role() == "assistant"
        });
        assert!(
            !paired,
            "an assistant message without reasoning must not be followed by another assistant message"
        );
        let last = messages.last().expect("the request carries messages");
        assert!(
            !(last.role() == "assistant" && last.reasoning().is_none()),
            "the request must not end on an assistant message without reasoning"
        );
    }
}
