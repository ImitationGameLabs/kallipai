use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use just_llm_client::tools::LlmTool;

use super::super::backend::ShellBackend;

/// Arguments accepted by [`ShellSessionRestart`].
#[derive(Debug, Deserialize, Serialize)]
pub struct RestartArgs {
    /// Session name to restart.
    pub name: String,
    /// Whether to recreate the session with a clean environment.
    #[serde(default)]
    pub clean_env: bool,
}

/// Result returned by [`ShellSessionRestart`].
#[derive(Debug, Deserialize, Serialize)]
pub struct RestartOutput {
    /// Restarted session name.
    pub name: String,
    /// Whether the environment was reset.
    pub clean_env: bool,
}

/// Tool that restarts shell sessions.
pub struct ShellSessionRestart<B: ShellBackend> {
    backend: Arc<Mutex<B>>,
}

impl<B: ShellBackend> ShellSessionRestart<B> {
    /// Creates a new tool.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: ShellBackend + Send + Sync + 'static> LlmTool for ShellSessionRestart<B> {
    fn name(&self) -> &str {
        "shell_session_restart"
    }

    fn description(&self) -> &str {
        "Restart a shell session, optionally with a clean environment."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Session name to restart."
                },
                "clean_env": {
                    "type": "boolean",
                    "description": "If true, recreate the session with a clean environment.",
                    "default": false
                }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, args_json: &str) -> anyhow::Result<String> {
        let args: RestartArgs = serde_json::from_str(args_json)?;
        let mut backend = self.backend.lock().await;
        backend.restart_session(&args.name, args.clean_env).await?;

        Ok(serde_json::to_string(&RestartOutput {
            name: args.name,
            clean_env: args.clean_env,
        })?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::shell::{MockShellBackend, ShellError};

    #[tokio::test]
    async fn restart_session_resets_session() {
        let backend = Arc::new(Mutex::new(MockShellBackend::new()));
        let tool = ShellSessionRestart::new(backend.clone());

        let result: RestartOutput = serde_json::from_str(
            &tool
                .call(r#"{"name":"main","clean_env":true}"#)
                .await
                .unwrap(),
        )
        .unwrap();

        assert!(result.clean_env);
        assert!(backend.lock().await.has_session("main"));
    }

    #[tokio::test]
    async fn restart_session_propagates_missing_session_errors() {
        let backend = Arc::new(Mutex::new(MockShellBackend::new()));
        let tool = ShellSessionRestart::new(backend);

        let error = tool.call(r#"{"name":"missing"}"#).await.unwrap_err();

        assert!(error.is::<ShellError>());
    }
}
