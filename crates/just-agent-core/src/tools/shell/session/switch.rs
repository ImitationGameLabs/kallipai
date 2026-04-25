use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use just_llm_client::tools::LlmTool;

use super::super::backend::ShellBackend;

/// Arguments accepted by [`ShellSessionSwitch`].
#[derive(Debug, Deserialize, Serialize)]
pub struct SwitchArgs {
    /// Target session name.
    pub name: String,
}

/// Result returned by [`ShellSessionSwitch`].
#[derive(Debug, Deserialize, Serialize)]
pub struct SwitchOutput {
    /// Switched-to session.
    pub name: String,
    /// Previously focused session.
    pub previous_session: String,
}

/// Tool that switches shell session focus.
pub struct ShellSessionSwitch<B: ShellBackend> {
    backend: Arc<Mutex<B>>,
}

impl<B: ShellBackend> ShellSessionSwitch<B> {
    /// Creates a new tool.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: ShellBackend + Send + Sync + 'static> LlmTool for ShellSessionSwitch<B> {
    fn name(&self) -> &str {
        "shell_session_switch"
    }

    fn description(&self) -> &str {
        "Switch the focused shell session."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Target session name."
                }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, args_json: &str) -> anyhow::Result<String> {
        let args: SwitchArgs = serde_json::from_str(args_json)?;
        let mut backend = self.backend.lock().await;
        let previous_session = backend.current_session().to_owned();
        backend.switch_session(&args.name).await?;

        Ok(serde_json::to_string(&SwitchOutput {
            name: args.name,
            previous_session,
        })?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::shell::{MockShellBackend, ShellError};

    #[tokio::test]
    async fn switch_session_changes_focus() {
        let backend = Arc::new(Mutex::new(MockShellBackend::new()));
        backend.lock().await.add_session("worker");
        let tool = ShellSessionSwitch::new(backend.clone());

        let result: SwitchOutput =
            serde_json::from_str(&tool.call(r#"{"name":"worker"}"#).await.unwrap()).unwrap();

        assert_eq!(result.previous_session, "main");
        assert_eq!(backend.lock().await.current_session(), "worker");
    }

    #[tokio::test]
    async fn switch_session_propagates_missing_session_errors() {
        let backend = Arc::new(Mutex::new(MockShellBackend::new()));
        let tool = ShellSessionSwitch::new(backend);

        let error = tool.call(r#"{"name":"missing"}"#).await.unwrap_err();

        assert!(error.is::<ShellError>());
    }
}
