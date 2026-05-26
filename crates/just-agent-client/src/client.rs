use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use just_agent_core::types::SseEvent;
use just_llm_client::JsonEventStream;

use crate::types::*;

struct Inner {
    base_url: String,
    http: reqwest::Client,
}

/// Async client for the just-agent daemon HTTP API.
#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<Inner>,
}

impl DaemonClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            inner: Arc::new(Inner {
                base_url: base_url.trim_end_matches('/').to_owned(),
                http: reqwest::Client::new(),
            }),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.inner.base_url)
    }

    /// Spawn a new agent instance on the daemon.
    pub async fn spawn(
        &self,
        workspace_root: Option<String>,
        skills: Vec<String>,
        prompt: Option<String>,
    ) -> Result<String> {
        let resp: CreateAgentResponse = self
            .inner
            .http
            .post(self.url("/agents"))
            .json(&CreateAgentRequest { workspace_root, skills, prompt })
            .send()
            .await
            .context("failed to connect to daemon")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse response")?;
        Ok(resp.id)
    }

    /// Send a prompt and wait for the agent to finish (with timeout).
    ///
    /// Subscribes to SSE before posting the prompt, then waits for a terminal
    /// event (`Finished` / `Error` / `MaxRoundsExceeded`). Intermediate delta
    /// events are intentionally ignored — this is a request-response API, not
    /// a streaming display. Use [`Self::event_stream`] for real-time consumption.
    pub async fn send_prompt(&self, id: &str, prompt: &str, timeout_secs: u64) -> Result<String> {
        // Subscribe to SSE before sending prompt to avoid race condition.
        let sse_response = self
            .inner
            .http
            .get(self.url(&format!("/agents/{id}/events")))
            .send()
            .await
            .context("failed to subscribe to agent events")?;

        let mut stream = JsonEventStream::<SseEvent>::from_response(sse_response)
            .context("failed to parse SSE stream")?;

        // Send the prompt.
        self.inner
            .http
            .post(self.url(&format!("/agents/{id}/prompt")))
            .json(&PromptRequest { text: prompt.to_owned() })
            .send()
            .await
            .context("failed to send prompt")?
            .error_for_status()
            .context("daemon returned error")?;

        // Wait for completion with timeout.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("timed out waiting for agent {id} after {timeout_secs}s");
            }

            match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(Ok(event))) => match event {
                    SseEvent::Finished { content } => return Ok(content),
                    SseEvent::Error { message } => anyhow::bail!("agent error: {message}"),
                    SseEvent::MaxRoundsExceeded => {
                        anyhow::bail!("agent {id} exceeded maximum tool rounds")
                    }
                    SseEvent::Cancelled => anyhow::bail!("agent {id} was cancelled"),
                    _ => continue,
                },
                Ok(Some(Err(e))) => anyhow::bail!("SSE error: {e}"),
                Ok(None) => anyhow::bail!("agent {id} event stream closed unexpectedly"),
                Err(_) => anyhow::bail!("timed out waiting for agent {id} after {timeout_secs}s"),
            }
        }
    }

    /// Fire-and-forget prompt POST. Use when already consuming the SSE stream.
    pub async fn post_prompt(&self, id: &str, text: &str) -> Result<()> {
        self.inner
            .http
            .post(self.url(&format!("/agents/{id}/prompt")))
            .json(&PromptRequest { text: text.to_owned() })
            .send()
            .await
            .context("failed to send prompt")?
            .error_for_status()
            .context("daemon returned error")?;
        Ok(())
    }

    /// List all agent instances.
    pub async fn list_agents(&self) -> Result<Vec<AgentSummary>> {
        let resp: ListAgentsResponse = self
            .inner
            .http
            .get(self.url("/agents"))
            .send()
            .await
            .context("failed to connect to daemon")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse response")?;
        Ok(resp.agents)
    }

    /// Kill an agent instance.
    pub async fn kill_agent(&self, id: &str) -> Result<()> {
        self.inner
            .http
            .delete(self.url(&format!("/agents/{id}")))
            .send()
            .await
            .context("failed to connect to daemon")?
            .error_for_status()
            .context("daemon returned error")?;
        Ok(())
    }

    /// Interrupt the current agent operation gracefully.
    pub async fn interrupt_agent(&self, id: &str) -> Result<()> {
        self.inner
            .http
            .post(self.url(&format!("/agents/{id}/interrupt")))
            .send()
            .await
            .context("failed to connect to daemon")?
            .error_for_status()
            .context("daemon returned error")?;
        Ok(())
    }

    /// Get a raw SSE event stream for the given agent.
    pub async fn event_stream(&self, id: &str) -> Result<JsonEventStream<SseEvent>> {
        let response = self
            .inner
            .http
            .get(self.url(&format!("/agents/{id}/events")))
            .send()
            .await
            .context("failed to subscribe to agent events")?;
        JsonEventStream::from_response(response).context("failed to parse SSE stream")
    }

    /// Send an approval decision for a deferred action.
    pub async fn respond_approval(
        &self,
        id: &str,
        request_id: &str,
        decision: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        self.inner
            .http
            .post(self.url(&format!("/agents/{id}/approval")))
            .json(&ApprovalRequestBody {
                request_id: request_id.to_owned(),
                decision: decision.to_owned(),
                reason: reason.map(|s| s.to_owned()),
            })
            .send()
            .await
            .context("failed to connect to daemon")?
            .error_for_status()
            .context("daemon returned error")?;
        Ok(())
    }

    /// Get agent status including context usage and retry history.
    pub async fn agent_status(&self, id: &str) -> Result<AgentStatusResponse> {
        let status = self
            .inner
            .http
            .get(self.url(&format!("/agents/{id}/status")))
            .send()
            .await
            .context("failed to get agent status")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse status response")?;
        Ok(status)
    }
}
