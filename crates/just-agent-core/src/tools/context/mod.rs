//! Context management tools for just-agent.
//!
//! Provides tools for pinning and unpinning content in the agent's
//! persistent context layer, checking token usage, and evicting old turns.

mod evict;
mod pin;
mod status;
mod unpin;

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::context::AgenticContext;

/// Creates context management tools sharing the same backing store.
pub fn context_tool_set(
    ctx: Arc<Mutex<dyn AgenticContext>>,
) -> Vec<Box<dyn just_llm_client::tools::LlmTool>> {
    vec![
        Box::new(pin::ContextPinTool::new(ctx.clone())),
        Box::new(unpin::ContextUnpinTool::new(ctx.clone())),
        Box::new(status::ContextStatusTool::new(ctx.clone())),
        Box::new(evict::ContextEvictTool::new(ctx)),
    ]
}
