use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use just_llm_client::tools::LlmTool;

use super::super::backend::ShellBackend;

/// Arguments accepted by [`ShellSessionKill`].
#[derive(Debug, Deserialize, Serialize)]
pub struct KillArgs {
    /// Session name to kill.
    pub name: String,
}

/// Result returned by [`ShellSessionKill`].
#[derive(Debug, Deserialize, Serialize)]
pub struct KillOutput {
    /// Killed session name.
    pub name: String,
    /// Whether it was focused at the time.
    pub was_current: bool,
    /// New focused session when the killed session was current.
    pub new_current: Option<String>,
}

/// Tool that kills shell sessions.
pub struct ShellSessionKill<B: ShellBackend> {
    backend: Arc<Mutex<B>>,
}

impl<B: ShellBackend> ShellSessionKill<B> {
    /// Creates a new tool.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: ShellBackend + Send + Sync + 'static> LlmTool for ShellSessionKill<B> {
    fn name(&self) -> &str {
        "shell_session_kill"
    }

    fn description(&self) -> &str {
        "Kill a shell session and all processes running inside it."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Session name to kill."
                }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, args_json: &str) -> anyhow::Result<String> {
        let args: KillArgs = serde_json::from_str(args_json)?;
        let mut backend = self.backend.lock().await;
        let was_current = backend.current_session() == args.name;
        backend.kill_session(&args.name).await?;
        let new_current = was_current.then(|| backend.current_session().to_owned());

        Ok(serde_json::to_string(&KillOutput {
            name: args.name,
            was_current,
            new_current,
        })?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::shell::{MockShellBackend, ShellError};

    #[tokio::test]
    async fn kill_session_removes_session() {
        let backend = Arc::new(Mutex::new(MockShellBackend::new()));
        backend.lock().await.add_session("worker");
        let tool = ShellSessionKill::new(backend.clone());

        let result: KillOutput =
            serde_json::from_str(&tool.call(r#"{"name":"worker"}"#).await.unwrap()).unwrap();

        assert_eq!(result.name, "worker");
        assert!(!backend.lock().await.has_session("worker"));
    }

    #[tokio::test]
    async fn kill_session_propagates_missing_session_errors() {
        let backend = Arc::new(Mutex::new(MockShellBackend::new()));
        let tool = ShellSessionKill::new(backend);

        let error = tool.call(r#"{"name":"missing"}"#).await.unwrap_err();

        assert!(error.is::<ShellError>());
    }
}
