use std::sync::Arc;

use anyhow::{Context, Result};
use just_agent_common::agentid::AgentId;
use just_agent_common::protocol::SseEvent;
use just_llm_client::JsonEventStream;

use crate::types::{ListApprovalsParams, MessageRequest};
use crate::{
    AgentPermissionsResponse, AgentStatusResponse, AgentSummary, ApprovalDecisionBody,
    ApprovalEntry, CreateAgentRequest, CreateAgentResponse, ListAgentsResponse,
    ListApprovalsResponse, ToolPolicy,
};

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

    /// Send a decision (approve/deny) for an approval.
    pub async fn respond_approval(
        &self,
        approval_id: &str,
        decision: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        self.with_auth(
            self.inner
                .http
                .post(self.url(&format!("/approvals/{approval_id}")))
                .json(&ApprovalDecisionBody {
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

    /// List approvals with optional filtering and pagination.
    pub async fn list_approvals(
        &self,
        params: &ListApprovalsParams,
    ) -> Result<ListApprovalsResponse> {
        let req = self.inner.http.get(self.url("/approvals")).query(params);

        let resp: ListApprovalsResponse = self
            .with_auth(req)
            .send()
            .await
            .context("failed to connect to daemon")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse response")?;
        Ok(resp)
    }

    /// Get a single approval by id.
    pub async fn get_approval(&self, id: &str) -> Result<ApprovalEntry> {
        let req = self.inner.http.get(self.url(&format!("/approvals/{id}")));
        let entry: ApprovalEntry = self
            .with_auth(req)
            .send()
            .await
            .context("failed to connect to daemon")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse response")?;
        Ok(entry)
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

    /// Get agent permission profile and tool policy rules.
    pub async fn agent_permissions(&self, id: &AgentId) -> Result<AgentPermissionsResponse> {
        let perms = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/permissions"))),
            )
            .send()
            .await
            .context("failed to get agent permissions")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse permissions response")?;
        Ok(perms)
    }

    /// Get the raw tool policy for an agent.
    pub async fn get_policy(&self, id: &AgentId) -> Result<ToolPolicy> {
        let policy = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/policy"))),
            )
            .send()
            .await
            .context("failed to get agent policy")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse policy response")?;
        Ok(policy)
    }

    /// Update the tool policy for an agent.
    pub async fn update_policy(&self, id: &AgentId, policy: &ToolPolicy) -> Result<()> {
        self.with_auth(
            self.inner
                .http
                .put(self.url(&format!("/agents/{id}/policy")))
                .json(policy),
        )
        .send()
        .await
        .context("failed to update agent policy")?
        .error_for_status()
        .context("daemon returned error")?;
        Ok(())
    }
}
