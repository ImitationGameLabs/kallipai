//! Agent lifecycle methods for [`TagmaClient`].
//!
//! Spawning, messaging, metadata, and lifecycle control — the `/agents`
//! resource domain. Split from `client.rs` verbatim; the client core
//! (auth, transport, response handling) stays in the parent module.

use super::TagmaClient;
use crate::types::MessageRequest;
use anyhow::{Context, Result};
use just_llm_client::JsonEventStream;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{
    AgentSummary, CreateAgentRequest, CreateAgentResponse, ListAgentsResponse, SseEvent,
    UpdateActivityRequest, UpdateAgentMetadataRequest,
};

impl TagmaClient {
    /// Spawn a new agent instance on the tagma.
    pub async fn spawn(&self, req: CreateAgentRequest) -> Result<AgentId> {
        let resp: CreateAgentResponse = self
            .handle_response(
                self.with_auth(self.inner.http.post(self.url("/agents")).json(&req))
                    .send()
                    .await
                    .context("failed to connect to tagma")?,
                "failed to parse response",
            )
            .await?;
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
        self.handle_response(
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
            .context("failed to send message")?,
            "failed to parse message response",
        )
        .await
    }
    /// Fetch the tagma's single root agent. The tagma eagerly creates one
    /// root at startup (see `ensure_root_agent`), so this always succeeds once
    /// the tagma is accepting connections.
    pub async fn get_root_agent(&self) -> Result<AgentSummary> {
        self.handle_response(
            self.with_auth(self.inner.http.get(self.url("/agents/root")))
                .send()
                .await
                .context("failed to connect to tagma")?,
            "failed to parse response",
        )
        .await
    }

    /// List agent instances. Pass `created_by = Some(sup)` to list only a
    /// superior's direct subagents; `None` lists all agents.
    pub async fn list_agents(&self, created_by: Option<&AgentId>) -> Result<Vec<AgentSummary>> {
        let mut req = self.with_auth(self.inner.http.get(self.url("/agents")));
        if let Some(sup) = created_by {
            req = req.query(&[("created_by", sup.to_string())]);
        }
        let resp: ListAgentsResponse = self
            .handle_response(
                req.send().await.context("failed to connect to tagma")?,
                "failed to parse response",
            )
            .await?;
        Ok(resp.agents)
    }

    /// Update an agent's `role` and/or `description`. Caller must be the agent's
    /// direct supervisor (or operator). `None` fields are left unchanged.
    pub async fn update_agent_metadata(
        &self,
        id: &AgentId,
        body: UpdateAgentMetadataRequest,
    ) -> Result<AgentSummary> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .put(self.url(&format!("/agents/{id}/metadata")))
                    .json(&body),
            )
            .send()
            .await
            .context("failed to connect to tagma")?,
            "failed to parse response",
        )
        .await
    }

    /// Report an agent's current activity. Caller must be the agent itself (or
    /// operator) — activity is self-reported. An empty `activity` clears it.
    pub async fn update_activity(&self, id: &AgentId, body: UpdateActivityRequest) -> Result<()> {
        self.ensure_success(
            self.with_auth(
                self.inner
                    .http
                    .put(self.url(&format!("/agents/{id}/activity")))
                    .json(&body),
            )
            .send()
            .await
            .context("failed to connect to tagma")?,
        )
        .await?;
        Ok(())
    }

    /// Remove an agent instance.
    /// Requires superior-level auth if the tagma enforces it.
    pub async fn remove_agent(&self, id: &AgentId) -> Result<()> {
        self.ensure_success(
            self.with_auth(self.inner.http.delete(self.url(&format!("/agents/{id}"))))
                .send()
                .await
                .context("failed to connect to tagma")?,
        )
        .await?;
        Ok(())
    }

    /// Interrupt the current agent operation gracefully.
    /// Requires superior-level auth if the tagma enforces it.
    pub async fn interrupt_agent(&self, id: &AgentId) -> Result<()> {
        self.ensure_success(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/agents/{id}/interrupt"))),
            )
            .send()
            .await
            .context("failed to connect to tagma")?,
        )
        .await?;
        Ok(())
    }

    /// Get a raw SSE event stream for the given agent.
    pub async fn event_stream(&self, id: &AgentId) -> Result<JsonEventStream<SseEvent>> {
        let response = self
            .ensure_success(
                self.with_auth(
                    self.inner
                        .http
                        .get(self.url(&format!("/agents/{id}/events"))),
                )
                .send()
                .await
                .context("failed to subscribe to agent events")?,
            )
            .await?;
        JsonEventStream::from_response(response).context("failed to parse SSE stream")
    }
}
