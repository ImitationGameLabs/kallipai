//! Read accumulated output from a background task.

use std::sync::Arc;

use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::backend::ShellBackend;
use crate::supervisor::TaskState;

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
    /// Task state, serialized as `"running"` / `"exited"` / `"killed"`.
    pub state: TaskState,
    /// Exit code once exited, else null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// `true` if the task appears stalled on an interactive prompt.
    pub stalled: bool,
    /// Total bytes written so far.
    pub bytes: usize,
}

/// Tool that reads a background task's accumulated output.
pub struct BgRead<B: ShellBackend> {
    backend: Arc<Mutex<B>>,
}

impl<B: ShellBackend> BgRead<B> {
    /// Creates a new tool sharing `backend`.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: ShellBackend + Send + Sync + 'static> LlmTool for BgRead<B> {
    fn name(&self) -> &str {
        super::names::BG_READ
    }

    fn description(&self) -> &str {
        "Read the accumulated output and status of a background task started by bash_exec \
         (background:true, or a timed-out command converted at timeout). Poll while it is \
         running; when a `[Background task <id> <state>] notice arrives, call this to \
         collect the final output and exit code. If `stalled` is true the task appears to \
         be waiting on an interactive prompt that will never resolve on its own — kill it \
         with bash_background_kill."
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
            state: result.state,
            exit_code: result.exit_code,
            stalled: result.stalled,
            bytes: result.bytes,
        };
        Ok(serde_json::to_string(&output)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire contract for `state` is a bare lowercase string; pin all
    /// three variants so the String -> TaskState switch stays
    /// byte-identical on the wire.
    #[test]
    fn task_state_serializes_as_lowercase_wire_string() {
        assert_eq!(
            serde_json::to_string(&TaskState::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&TaskState::Exited).unwrap(),
            "\"exited\""
        );
        assert_eq!(
            serde_json::to_string(&TaskState::Killed).unwrap(),
            "\"killed\""
        );
    }

    #[test]
    fn bg_read_output_serializes_state_as_plain_string() {
        let out = BgReadOutput {
            task_id: "t-1".into(),
            output: "tail".into(),
            state: TaskState::Exited,
            exit_code: Some(0),
            stalled: false,
            bytes: 4,
        };
        let wire = serde_json::to_value(&out).unwrap();
        assert_eq!(wire["state"], "exited");
        assert_eq!(wire["task_id"], "t-1");
        assert_eq!(wire["exit_code"], 0);
    }
}
