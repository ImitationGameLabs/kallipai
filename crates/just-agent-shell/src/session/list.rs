use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use just_llm_client::tools::LlmTool;

use super::super::backend::{SessionInfo, ShellBackend};

/// Arguments accepted by [`ShellSessionList`].
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ListArgs {}

/// Result returned by [`ShellSessionList`].
#[derive(Debug, Deserialize, Serialize)]
pub struct ListOutput {
    /// Known sessions.
    pub sessions: Vec<SessionInfo>,
    /// Focused session name.
    pub current_session: String,
}

/// Tool that lists shell sessions.
pub struct ShellSessionList<B: ShellBackend> {
    backend: Arc<Mutex<B>>,
}

impl<B: ShellBackend> ShellSessionList<B> {
    /// Creates a new tool.
    pub fn new(backend: Arc<Mutex<B>>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: ShellBackend + Send + Sync + 'static> LlmTool for ShellSessionList<B> {
    fn name(&self) -> &str {
        super::names::LIST
    }

    fn description(&self) -> &str {
        "List all available shell sessions."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn call(&self, args_json: &str) -> anyhow::Result<String> {
        let _: ListArgs = serde_json::from_str(args_json)?;
        let backend = self.backend.lock().await;
        let sessions = backend.list_sessions().await?;
        let current_session = backend.current_session().to_owned();

        Ok(serde_json::to_string(&ListOutput {
            sessions,
            current_session,
        })?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockShellBackend;

    #[tokio::test]
    async fn list_sessions_returns_current_session() {
        let backend = Arc::new(Mutex::new(MockShellBackend::new()));
        backend.lock().await.add_session("worker");
        let tool = ShellSessionList::new(backend);

        let result: ListOutput = serde_json::from_str(&tool.call("{}").await.unwrap()).unwrap();

        assert_eq!(result.current_session, "main");
        assert_eq!(result.sessions.len(), 2);
    }
}
