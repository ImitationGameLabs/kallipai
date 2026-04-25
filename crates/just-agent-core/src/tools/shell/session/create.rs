use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use just_llm_client::tools::LlmTool;

use super::super::backend::ShellBackend;

/// Arguments accepted by [`ShellSessionCreate`].
#[derive(Debug, Deserialize, Serialize)]
pub struct CreateArgs {
    /// Name of the new session.
    pub name: String,
    /// Working directory for the session.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// Result returned by [`ShellSessionCreate`].
#[derive(Debug, Deserialize, Serialize)]
pub struct CreateOutput {
    /// Created session name.
    pub name: String,
    /// Working directory assigned to the session.
    pub cwd: String,
}

/// Tool that creates shell sessions.
pub struct ShellSessionCreate<B: ShellBackend> {
    backend: Arc<Mutex<B>>,
}

impl<B: ShellBackend> ShellSessionCreate<B> {
    /// Creates a new tool.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: ShellBackend + Send + Sync + 'static> LlmTool for ShellSessionCreate<B> {
    fn name(&self) -> &str {
        "shell_session_create"
    }

    fn description(&self) -> &str {
        "Create a new persistent shell session."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Unique session name."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory. Defaults to the current directory."
                }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, args_json: &str) -> anyhow::Result<String> {
        let args: CreateArgs = serde_json::from_str(args_json)?;
        let mut backend = self.backend.lock().await;
        backend
            .create_session(&args.name, args.cwd.as_deref())
            .await?;

        let cwd = args
            .cwd
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "/tmp".to_owned())
            });

        Ok(serde_json::to_string(&CreateOutput {
            name: args.name,
            cwd,
        })?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::shell::{MockShellBackend, ShellError};

    fn create_tool_with_mock() -> (
        ShellSessionCreate<MockShellBackend>,
        Arc<Mutex<MockShellBackend>>,
    ) {
        let mock = Arc::new(Mutex::new(MockShellBackend::new()));
        let tool = ShellSessionCreate::new(mock.clone());
        (tool, mock)
    }

    #[tokio::test]
    async fn create_session_creates_named_session() {
        let (tool, mock) = create_tool_with_mock();

        let result: CreateOutput =
            serde_json::from_str(&tool.call(r#"{"name":"worker"}"#).await.unwrap()).unwrap();

        assert_eq!(result.name, "worker");
        assert!(mock.lock().await.has_session("worker"));
    }

    #[tokio::test]
    async fn create_session_rejects_duplicates() {
        let (tool, _) = create_tool_with_mock();

        let error = tool.call(r#"{"name":"main"}"#).await.unwrap_err();

        assert!(error.is::<ShellError>());
    }
}
