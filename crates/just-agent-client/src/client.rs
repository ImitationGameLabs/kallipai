use std::sync::Arc;

use anyhow::{Context, Result};
use just_agent_common::agentid::AgentId;
use just_agent_common::protocol::SseEvent;
use just_llm_client::JsonEventStream;

use crate::types::{ListApprovalsParams, MessageRequest};
use crate::{
    AgentPermissionsResponse, AgentStatusResponse, AgentSummary, ApprovalDecisionBody,
    ApprovalEntry, CreateAgentRequest, CreateAgentResponse, ListAgentsResponse,
    ListApprovalsResponse, ListSkillPromoteRecordsResponse, PromoteDecision, SkillMeta,
    SkillPathsResponse, SkillPromoteDecisionBody, SkillPromoteShowResponse,
    SkillPromoteSubmitResponse, ToolPolicy,
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

    /// Send a message to an agent. Returns queue depth feedback.
    ///
    /// - `queue_depth == 0`: agent will process the message immediately.
    /// - `queue_depth > 0`: message is queued behind existing messages (warning included).
    /// - Returns an error on 503 if the message queue is full.
    pub async fn post_message(
        &self,
        id: &AgentId,
        text: &str,
    ) -> Result<crate::types::MessageResponse> {
        let response = self
            .with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/agents/{id}/message")))
                    .json(&MessageRequest {
                        text: text.to_owned(),
                    }),
            )
            .send()
            .await
            .context("failed to send message")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("daemon returned {status}: {body}"));
        }

        response
            .json()
            .await
            .context("failed to parse message response")
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

    /// Get skill directory paths for an agent (shared + local).
    pub async fn skill_paths(&self, id: &AgentId) -> Result<SkillPathsResponse> {
        let resp = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/skills/paths"))),
            )
            .send()
            .await
            .context("failed to get skill paths")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse skill paths response")?;
        Ok(resp)
    }

    /// Get skill metadata (name + description) for a specific skill.
    ///
    /// The skill name is URL-encoded so that nested paths like
    /// `code/refactoring` survive as a single path segment.
    pub async fn skill_meta(&self, id: &AgentId, name: &str) -> Result<SkillMeta> {
        let encoded = name.replace('/', "%2F");
        let resp = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/skills/{encoded}/meta"))),
            )
            .send()
            .await
            .context("failed to get skill meta")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse skill meta response")?;
        Ok(resp)
    }

    // -----------------------------------------------------------------------
    // Skill promote request (review-based promote flow)
    // -----------------------------------------------------------------------

    /// Submit a promote request for a local skill.
    pub async fn submit_promote_request(
        &self,
        id: &AgentId,
        name: &str,
    ) -> Result<SkillPromoteSubmitResponse> {
        let encoded = name.replace('/', "%2F");
        let resp = self
            .with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/agents/{id}/skills/{encoded}/promote-request"))),
            )
            .send()
            .await
            .context("failed to submit promote request")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse promote submit response")?;
        Ok(resp)
    }

    /// List promote requests, optionally filtered by status.
    pub async fn list_promote_requests(
        &self,
        status: Option<&str>,
    ) -> Result<ListSkillPromoteRecordsResponse> {
        let mut req = self.inner.http.get(self.url("/skill-promote-requests"));
        if let Some(s) = status {
            req = req.query(&[("status", s)]);
        }
        let resp = self
            .with_auth(req)
            .send()
            .await
            .context("failed to list promote requests")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse promote list response")?;
        Ok(resp)
    }

    /// Show a promote request with full old/new content for diff review.
    pub async fn show_promote_request(&self, id: &str) -> Result<SkillPromoteShowResponse> {
        let resp = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/skill-promote-requests/{id}"))),
            )
            .send()
            .await
            .context("failed to show promote request")?
            .error_for_status()
            .context("daemon returned error")?
            .json()
            .await
            .context("failed to parse promote show response")?;
        Ok(resp)
    }

    /// Approve or deny a promote request.
    pub async fn respond_promote_request(
        &self,
        id: &str,
        decision: PromoteDecision,
        reason: Option<&str>,
    ) -> Result<()> {
        self.with_auth(
            self.inner
                .http
                .post(self.url(&format!("/skill-promote-requests/{id}")))
                .json(&SkillPromoteDecisionBody {
                    decision,
                    reason: reason.map(|s| s.to_owned()),
                }),
        )
        .send()
        .await
        .context("failed to respond to promote request")?
        .error_for_status()
        .context("daemon returned error")?;
        Ok(())
    }
}
