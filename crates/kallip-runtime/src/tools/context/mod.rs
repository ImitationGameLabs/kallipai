//! Context management tools for kallip.
//!
//! Provides tools for pinning and unpinning content in the agent's
//! persistent context layer, checking token usage, and evicting old turns.

mod evict;
mod exec_policy;
mod pin;
mod pin_last;
mod status;
mod unpin;

pub use evict::ContextEvictTool;
pub use exec_policy::ExecPolicyTool;
pub use pin::ContextPinTool;
pub use pin_last::ContextPinLastTool;
pub use status::ContextStatusTool;
pub use unpin::ContextUnpinTool;

use std::sync::{Arc, RwLock};

use kallip_common::policy::ExecPolicy;
use tokio::sync::Mutex;

use crate::context::AgenticContext;

/// Creates context management tools sharing the same backing store.
pub fn context_tool_set(
    ctx: Arc<Mutex<dyn AgenticContext>>,
    exec_policy: Arc<RwLock<ExecPolicy>>,
) -> Vec<Box<dyn just_llm_client::tools::LlmTool>> {
    vec![
        Box::new(pin::ContextPinTool::new(ctx.clone())),
        Box::new(pin_last::ContextPinLastTool::new(ctx.clone())),
        Box::new(unpin::ContextUnpinTool::new(ctx.clone())),
        Box::new(status::ContextStatusTool::new(ctx.clone())),
        Box::new(evict::ContextEvictTool::new(ctx.clone())),
        Box::new(exec_policy::ExecPolicyTool::new(exec_policy)),
    ]
}
