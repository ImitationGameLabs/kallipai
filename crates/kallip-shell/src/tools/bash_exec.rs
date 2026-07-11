//! Command-execution tool.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::backend::{CaptureMode, DEFAULT_TIMEOUT_SECS, ShellBackend};

/// Arguments accepted by [`BashExec`].
#[derive(Debug, Deserialize, Serialize)]
pub struct BashExecArgs {
    /// Shell command to execute.
    pub command: String,
    /// Timeout in seconds. Defaults to 120.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Run in the background (returns a task id immediately).
    #[serde(default)]
    pub background: bool,
    /// How to capture output: `"merged"` (default; stdout+stderr interleaved as
    /// one stream), `"separate"` (stdout+stderr as two fields), `"stdout"`, or
    /// `"stderr"`.
    #[serde(default)]
    pub capture: CaptureMode,
}

/// Result returned by [`BashExec`]. Exactly the output field(s) for the
/// requested [`CaptureMode`] are present (the others are omitted on the wire):
/// `merged` -> `output`; `separate` -> `stdout` + `stderr`; `stdout` ->
/// `stdout`; `stderr` -> `stderr`.
#[derive(Debug, Deserialize, Serialize)]
pub struct BashExecOutput {
    /// Merged stdout+stderr (clipped to a tail on overflow). Present under
    /// `capture: "merged"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Captured stdout (clipped to a tail on overflow). Present under
    /// `capture: "separate"` or `"stdout"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// Captured stderr (clipped to a tail on overflow). Present under
    /// `capture: "separate"` or `"stderr"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    /// Exit code, or `None` on signal death; `124` on timeout.
    pub exit_code: Option<i32>,
    /// Whether the command exceeded its timeout.
    pub timed_out: bool,
    /// Whether a returned stream was clipped.
    pub truncated: bool,
    /// Working directory after the command (read fresh from `pwd`).
    pub cwd: String,
    /// Set when `background` was true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

/// Tool that executes commands against a [`ShellBackend`].
pub struct BashExec<B: ShellBackend> {
    backend: Arc<Mutex<B>>,
}

impl<B: ShellBackend> BashExec<B> {
    /// Creates a new tool sharing `backend`.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: ShellBackend + Send + Sync + 'static> LlmTool for BashExec<B> {
    fn name(&self) -> &str {
        super::names::BASH_EXEC
    }

    fn description(&self) -> &str {
        "Execute a shell command in a fresh, isolated bash process. By default stdout and \
         stderr are merged into one stream (`output`), like 2>&1, matching how a command \
         appears in a terminal; the command is responsible for any ordering between the two \
         (it must flush to enforce it). Use `capture` to return them separately or keep only \
         one stream: \"merged\" (default), \"separate\", \"stdout\", or \"stderr\". Also returns \
         the exit code and the working directory after the command (reflects cd). The working \
         directory is NOT sticky across calls — to operate in a directory, prepend \
         `cd <dir> &&` to your command and read the returned cwd. Supports a timeout \
         (default 120s) and optional background mode. A timed-out command is killed and \
         returns exit code 124."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds. Defaults to 120.",
                    "default": 120
                },
                "background": {
                    "type": "boolean",
                    "description": "If true, run in the background and return a task_id immediately.",
                    "default": false
                },
                "capture": {
                    "type": "string",
                    "enum": ["merged", "separate", "stdout", "stderr"],
                    "default": "merged",
                    "description": "How to capture output. \"merged\" (default) interleaves \
                    stdout and stderr into one stream (normal command experience). \"separate\" \
                    returns them as two fields. \"stdout\"/\"stderr\" keep only one stream."
                }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args_json: &str) -> anyhow::Result<String> {
        let args: BashExecArgs = serde_json::from_str(args_json)?;
        let timeout = Duration::from_secs(args.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));

        let mut backend = self.backend.lock().await;
        let output = if args.background {
            let task_id = backend.spawn_background(&args.command).await?;
            BashExecOutput {
                output: None,
                stdout: None,
                stderr: None,
                exit_code: None,
                timed_out: false,
                truncated: false,
                cwd: backend.cwd().to_string_lossy().into_owned(),
                task_id: Some(task_id),
            }
        } else {
            let result = backend.exec(&args.command, timeout, args.capture).await?;
            BashExecOutput {
                output: result.merged,
                stdout: result.stdout,
                stderr: result.stderr,
                exit_code: result.exit_code,
                timed_out: result.timed_out,
                truncated: result.truncated,
                cwd: result.cwd.to_string_lossy().into_owned(),
                task_id: None,
            }
        };

        Ok(serde_json::to_string(&output)?)
    }
}
