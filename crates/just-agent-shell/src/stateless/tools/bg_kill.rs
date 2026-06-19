//! Kill a background task.

use std::sync::Arc;

use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::stateless::backend::StatelessBackend;

/// Arguments accepted by [`BgKill`].
#[derive(Debug, Deserialize, Serialize)]
pub struct BgKillArgs {
    /// Background task id to cancel.
    pub task_id: String,
}

/// Result returned by [`BgKill`].
#[derive(Debug, Deserialize, Serialize)]
pub struct BgKillOutput {
    pub task_id: String,
}

/// Tool that cancels and reaps a background task.
pub struct BgKill<B: StatelessBackend> {
    backend: Arc<Mutex<B>>,
}

impl<B: StatelessBackend> BgKill<B> {
    /// Creates a new tool sharing `backend`.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: StatelessBackend + Send + Sync + 'static> LlmTool for BgKill<B> {
    fn name(&self) -> &str {
        super::names::BG_KILL
    }

    fn description(&self) -> &str {
        "Cancel and reap a background task started by bash_exec (background:true)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Background task id to cancel." }
            },
            "required": ["task_id"]
        })
    }

    async fn call(&self, args_json: &str) -> anyhow::Result<String> {
        let args: BgKillArgs = serde_json::from_str(args_json)?;
        let mut backend = self.backend.lock().await;
        backend.kill_background(&args.task_id).await?;
        Ok(serde_json::to_string(&BgKillOutput {
            task_id: args.task_id,
        })?)
    }
}
