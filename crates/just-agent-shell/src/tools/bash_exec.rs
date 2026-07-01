//! Command-execution tool.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use just_llm_client::tools::LlmTool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::backend::{DEFAULT_TIMEOUT_SECS, ShellBackend};

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
}

/// Result returned by [`BashExec`].
#[derive(Debug, Deserialize, Serialize)]
pub struct BashExecOutput {
    /// Captured stdout (clipped to a tail on overflow).
    pub stdout: String,
    /// Captured stderr (clipped to a tail on overflow).
    pub stderr: String,
    /// Exit code, or `None` on signal death; `124` on timeout.
    pub exit_code: Option<i32>,
    /// Whether the command exceeded its timeout.
    pub timed_out: bool,
    /// Whether stdout/stderr was clipped.
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
        "Execute a shell command in a fresh, isolated bash process. Returns stdout, stderr, \
         exit code, and the working directory after the command (reflects cd). Supports a \
         timeout (default 120s) and optional background mode. A timed-out command is killed \
         and returns exit code 124."
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
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                timed_out: false,
                truncated: false,
                cwd: backend.cwd().to_string_lossy().into_owned(),
                task_id: Some(task_id),
            }
        } else {
            let result = backend.exec(&args.command, timeout).await?;
            BashExecOutput {
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
