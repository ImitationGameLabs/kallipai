use std::sync::Arc;

use anyhow::{Context, Result};
use just_agent_core::types::AgentId;
use just_agent_core::types::SseEvent;
use just_llm_client::JsonEventStream;

use crate::types::*;

struct Inner {
    base_url: String,
    http: reqwest::Client,
    auth_token: Option<String>,
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
                auth_token: None,
            }),
        }
    }

    /// Creates a client that authenticates with the given auth token.
    pub fn new_with_token(base_url: &str, auth_token: String) -> Self {
        Self {
            inner: Arc::new(Inner {
                base_url: base_url.trim_end_matches('/').to_owned(),
                http: reqwest::Client::new(),
                auth_token: Some(auth_token),
            }),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.inner.base_url)
    }

    /// Set Authorization: Bearer <token> if an auth token is configured.
    fn with_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref token) = self.inner.auth_token {
            req.bearer_auth(token)
        } else {
            req
        }
    }

    /// Spawn a new agent instance on the daemon.
    pub async fn spawn(&self, req: CreateAgentRequest) -> Result<AgentId> {
        let resp: CreateAgentResponse = self
            .with_auth(self.inner.http.post(self.url("/agents")).json(&req))
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

    /// Fire-and-forget message POST. Use when already consuming the SSE stream.
    pub async fn post_message(&self, id: &AgentId, text: &str) -> Result<()> {
        self.with_auth(
            self.inner
                .http
                .post(self.url(&format!("/agents/{id}/message")))
                .json(&MessageRequest {
                    text: text.to_owned(),
                }),
        )
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
            .with_auth(self.inner.http.get(self.url("/agents")))
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

    /// Stop an agent instance.
    /// Requires superior-level auth if the daemon enforces it.
    pub async fn stop_agent(&self, id: &AgentId) -> Result<()> {
        self.with_auth(self.inner.http.delete(self.url(&format!("/agents/{id}"))))
            .send()
            .await
            .context("failed to connect to daemon")?
            .error_for_status()
            .context("daemon returned error")?;
        Ok(())
    }

    /// Interrupt the current agent operation gracefully.
    /// Requires superior-level auth if the daemon enforces it.
    pub async fn interrupt_agent(&self, id: &AgentId) -> Result<()> {
        self.with_auth(
            self.inner
                .http
                .post(self.url(&format!("/agents/{id}/interrupt"))),
        )
        .send()
        .await
        .context("failed to connect to daemon")?
        .error_for_status()
        .context("daemon returned error")?;
        Ok(())
    }

    /// Get a raw SSE event stream for the given agent.
    pub async fn event_stream(&self, id: &AgentId) -> Result<JsonEventStream<SseEvent>> {
        let response = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/events"))),
            )
            .send()
            .await
            .context("failed to subscribe to agent events")?;
        JsonEventStream::from_response(response).context("failed to parse SSE stream")
    }

    /// Send an approval decision for a deferred action.
    pub async fn respond_approval(
        &self,
        id: &AgentId,
        request_id: &str,
        decision: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        self.with_auth(
            self.inner
                .http
                .post(self.url(&format!("/agents/{id}/approval")))
                .json(&ApprovalRequestBody {
                    request_id: request_id.to_owned(),
                    decision: decision.to_owned(),
                    reason: reason.map(|s| s.to_owned()),
                }),
        )
        .send()
        .await
        .context("failed to connect to daemon")?
        .error_for_status()
        .context("daemon returned error")?;
        Ok(())
    }

    /// Get agent status including context usage and retry history.
    pub async fn agent_status(&self, id: &AgentId) -> Result<AgentStatusResponse> {
        let status = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/status"))),
            )
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
