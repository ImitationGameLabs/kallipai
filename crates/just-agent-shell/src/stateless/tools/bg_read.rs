//! Read accumulated output from a background task.

use std::sync::Arc;

use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::stateless::backend::StatelessBackend;

/// Default number of recent lines to return.
const DEFAULT_LINES: usize = 200;
/// Rough bytes-per-line budget for the tail read.
const BYTES_PER_LINE: usize = 256;

/// Arguments accepted by [`BgRead`].
#[derive(Debug, Deserialize, Serialize)]
pub struct BgReadArgs {
    /// Background task id returned by `bash_exec`.
    pub task_id: String,
    /// Number of recent lines to return. Defaults to 200.
    #[serde(default)]
    pub lines: Option<usize>,
}

/// Result returned by [`BgRead`].
#[derive(Debug, Deserialize, Serialize)]
pub struct BgReadOutput {
    pub task_id: String,
    /// Recent output (tail).
    pub output: String,
    /// Task state: `"running"` / `"exited"` / `"killed"`.
    pub state: String,
    /// Exit code once exited, else null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// `true` if the task appears stalled on an interactive prompt.
    pub stalled: bool,
    /// Total bytes written so far.
    pub bytes: usize,
}

/// Tool that reads a background task's accumulated output.
pub struct BgRead<B: StatelessBackend> {
    backend: Arc<Mutex<B>>,
}

impl<B: StatelessBackend> BgRead<B> {
    /// Creates a new tool sharing `backend`.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: StatelessBackend + Send + Sync + 'static> LlmTool for BgRead<B> {
    fn name(&self) -> &str {
        super::names::BG_READ
    }

    fn description(&self) -> &str {
        "Read the accumulated output and status of a background task started by bash_exec \
         (background:true). If `stalled` is true the task appears to be waiting on an \
         interactive prompt — kill it or feed it input."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Background task id." },
                "lines": {
                    "type": "integer",
                    "description": "Number of recent lines to return. Defaults to 200.",
                    "default": 200
                }
            },
            "required": ["task_id"]
        })
    }

    async fn call(&self, args_json: &str) -> anyhow::Result<String> {
        let args: BgReadArgs = serde_json::from_str(args_json)?;
        let lines = args.lines.unwrap_or(DEFAULT_LINES);
        let tail_bytes = lines.saturating_mul(BYTES_PER_LINE);

        let backend = self.backend.lock().await;
        let result = backend.read_background(&args.task_id, tail_bytes).await?;
        let output = BgReadOutput {
            task_id: args.task_id,
            output: result.output,
            state: result.state.as_str().to_owned(),
            exit_code: result.exit_code,
            stalled: result.stalled,
            bytes: result.bytes,
        };
        Ok(serde_json::to_string(&output)?)
    }
}
