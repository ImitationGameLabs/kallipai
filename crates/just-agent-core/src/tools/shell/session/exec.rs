//! Tool for executing shell commands inside a persistent session.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use just_llm_client::tools::LlmTool;

use super::super::backend::ShellBackend;

const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Arguments accepted by [`ShellSessionExec`].
#[derive(Debug, Deserialize, Serialize)]
pub struct ExecArgs {
    /// Shell command to execute.
    pub command: String,
    /// Target session name. Uses the current session when omitted.
    #[serde(default)]
    pub session: Option<String>,
    /// Timeout in seconds. Defaults to 120.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Whether to return immediately without waiting for completion.
    #[serde(default)]
    pub background: bool,
}

/// Result returned by [`ShellSessionExec`].
#[derive(Debug, Deserialize, Serialize)]
pub struct ExecOutput {
    /// Command output captured from the session.
    pub output: String,
    /// Exit code when known.
    pub exit_code: Option<i32>,
    /// Whether the command timed out.
    pub timed_out: bool,
    /// Session where the command ran.
    pub session: String,
}

/// Tool that executes commands against a shared shell backend.
pub struct ShellSessionExec<B: ShellBackend> {
    backend: Arc<Mutex<B>>,
}

impl<B: ShellBackend> ShellSessionExec<B> {
    /// Creates a new tool.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: ShellBackend + Send + Sync + 'static> LlmTool for ShellSessionExec<B> {
    fn name(&self) -> &str {
        "shell_session_exec"
    }

    fn description(&self) -> &str {
        "Execute a shell command in a persistent session. Supports timeouts and background mode."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute."
                },
                "session": {
                    "type": "string",
                    "description": "Optional target session name. Uses the current session when omitted."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds. Defaults to 120.",
                    "default": 120
                },
                "background": {
                    "type": "boolean",
                    "description": "If true, return immediately without waiting for command completion.",
                    "default": false
                }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args_json: &str) -> anyhow::Result<String> {
        let args: ExecArgs = serde_json::from_str(args_json)?;
        let timeout = Duration::from_secs(args.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));

        let mut backend = self.backend.lock().await;
        if let Some(session) = &args.session {
            backend.switch_session(session).await?;
        }

        let session = backend.current_session().to_owned();
        let result = backend
            .execute(&args.command, timeout, args.background)
            .await?;
        let output = ExecOutput {
            output: result.output,
            exit_code: result.exit_code,
            timed_out: result.timed_out,
            session,
        };

        Ok(serde_json::to_string(&output)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::shell::{MockShellBackend, ShellError};

    fn create_tool_with_mock() -> (
        ShellSessionExec<MockShellBackend>,
        Arc<Mutex<MockShellBackend>>,
    ) {
        let mock = Arc::new(Mutex::new(MockShellBackend::new()));
        let tool = ShellSessionExec::new(mock.clone());
        (tool, mock)
    }

    #[tokio::test]
    async fn bash_returns_output_and_exit_code() {
        let (tool, mock) = create_tool_with_mock();
        {
            let mut backend = mock.lock().await;
            backend.push_output("hello").push_exit_code(0);
        }

        let result: ExecOutput =
            serde_json::from_str(&tool.call(r#"{"command":"echo hello"}"#).await.unwrap()).unwrap();

        assert_eq!(result.output, "hello");
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.session, "main");
    }

    #[tokio::test]
    async fn bash_timeout_is_reported_in_band() {
        let (tool, mock) = create_tool_with_mock();
        mock.lock().await.set_should_timeout(true);

        let result: ExecOutput = serde_json::from_str(
            &tool
                .call(r#"{"command":"sleep 5","timeout":1}"#)
                .await
                .unwrap(),
        )
        .unwrap();

        assert!(result.timed_out);
        assert_eq!(result.exit_code, None);
    }

    #[tokio::test]
    async fn bash_switches_to_requested_session() {
        let (tool, mock) = create_tool_with_mock();
        {
            let mut backend = mock.lock().await;
            backend
                .add_session("worker")
                .push_output("done")
                .push_exit_code(0);
        }

        let result: ExecOutput = serde_json::from_str(
            &tool
                .call(r#"{"command":"echo done","session":"worker"}"#)
                .await
                .unwrap(),
        )
        .unwrap();

        assert_eq!(result.session, "worker");
    }

    #[tokio::test]
    async fn bash_propagates_missing_session_errors() {
        let (tool, _) = create_tool_with_mock();

        let error = tool
            .call(r#"{"command":"echo nope","session":"missing"}"#)
            .await
            .unwrap_err();

        assert!(error.is::<ShellError>());
    }
}
