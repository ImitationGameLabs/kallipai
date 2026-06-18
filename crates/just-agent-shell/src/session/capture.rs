//! Tool for capturing recent output from a shell session.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use just_llm_client::tools::LlmTool;

use super::super::backend::ShellBackend;

const DEFAULT_LINES: usize = 200;

/// Arguments accepted by [`ShellSessionCapture`].
#[derive(Debug, Deserialize, Serialize)]
pub struct CaptureArgs {
    /// Target session name. Uses the current session when omitted.
    #[serde(default)]
    pub session: Option<String>,
    /// Maximum number of lines to capture. Defaults to 200.
    #[serde(default)]
    pub lines: Option<usize>,
}

/// Result returned by [`ShellSessionCapture`].
#[derive(Debug, Deserialize, Serialize)]
pub struct CaptureOutput {
    /// Captured output text.
    pub output: String,
    /// Session the output came from.
    pub session: String,
    /// Number of captured lines returned.
    pub lines: usize,
}

/// Tool that captures recent output from a shared shell backend.
pub struct ShellSessionCapture<B: ShellBackend> {
    backend: Arc<Mutex<B>>,
}

impl<B: ShellBackend> ShellSessionCapture<B> {
    /// Creates a new tool.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: ShellBackend + Send + Sync + 'static> LlmTool for ShellSessionCapture<B> {
    fn name(&self) -> &str {
        super::names::CAPTURE
    }

    fn description(&self) -> &str {
        "Capture recent output from a shell session."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session": {
                    "type": "string",
                    "description": "Optional target session name. Uses the current session when omitted."
                },
                "lines": {
                    "type": "integer",
                    "description": "Maximum number of lines to capture. Defaults to 200.",
                    "default": 200
                }
            },
            "required": []
        })
    }

    async fn call(&self, args_json: &str) -> anyhow::Result<String> {
        let args: CaptureArgs = serde_json::from_str(args_json)?;
        let mut backend = self.backend.lock().await;

        if let Some(session) = &args.session {
            backend.switch_session(session).await?;
        }

        let session = backend.current_session().to_owned();
        let output = backend
            .capture_output(args.lines.unwrap_or(DEFAULT_LINES))
            .await?;
        let lines = output.lines().count();

        Ok(serde_json::to_string(&CaptureOutput {
            output,
            session,
            lines,
        })?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MockShellBackend, ShellError};

    fn create_tool_with_mock() -> (
        ShellSessionCapture<MockShellBackend>,
        Arc<Mutex<MockShellBackend>>,
    ) {
        let mock = Arc::new(Mutex::new(MockShellBackend::new()));
        let tool = ShellSessionCapture::new(mock.clone());
        (tool, mock)
    }

    #[tokio::test]
    async fn capture_output_returns_recent_lines() {
        let (tool, mock) = create_tool_with_mock();
        mock.lock()
            .await
            .set_session_output("main", vec!["line1", "line2", "line3"]);

        let result: CaptureOutput =
            serde_json::from_str(&tool.call(r#"{"lines":2}"#).await.unwrap()).unwrap();

        assert_eq!(result.output, "line2\nline3");
        assert_eq!(result.lines, 2);
    }

    #[tokio::test]
    async fn capture_output_can_switch_sessions() {
        let (tool, mock) = create_tool_with_mock();
        {
            let mut backend = mock.lock().await;
            backend
                .add_session("worker")
                .set_session_output("worker", vec!["worker output"]);
        }

        let result: CaptureOutput =
            serde_json::from_str(&tool.call(r#"{"session":"worker"}"#).await.unwrap()).unwrap();

        assert_eq!(result.session, "worker");
    }

    #[tokio::test]
    async fn capture_output_propagates_missing_session_errors() {
        let (tool, _) = create_tool_with_mock();

        let error = tool.call(r#"{"session":"missing"}"#).await.unwrap_err();

        assert!(error.is::<ShellError>());
    }
}
