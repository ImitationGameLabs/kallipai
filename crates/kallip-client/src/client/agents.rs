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
    AgentSummary, CreateAgentRequest, CreateAgentResponse, ListAgentsResponse,
    ProfileSetUpdateRequest, SseEvent, UpdateActivityRequest, UpdateAgentMetadataRequest,
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
    /// - `defer`: ask for run-boundary visibility on the receiver (wire `defer`).
    pub async fn post_message(
        &self,
        id: &AgentId,
        text: &str,
        defer: bool,
    ) -> Result<crate::types::MessageResponse> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/agents/{id}/message")))
                    .json(&MessageRequest {
                        text: text.to_owned(),
                        attachment: None,
                        defer,
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

    /// Resolve a CLI `<ID>` argument to an agent id. A UUID-shaped input is
    /// used verbatim; anything else is matched as an exact, case-sensitive
    /// role across the whole registry (unique by spawn-time discipline). No
    /// match lists the available roles; multiple holders (defensive — the
    /// tagma rejects duplicates) lists them. Resolution anchors the returned
    /// id, so a later rename cannot divert an in-flight command.
    pub async fn resolve_agent_ref(&self, input: &str) -> Result<AgentId> {
        if kallip_common::agentid::is_uuid_format(input) {
            return Ok(AgentId::from(input.to_string()));
        }
        let agents = self.list_agents(None).await?;
        let holders: Vec<&AgentSummary> = agents.iter().filter(|a| a.role == input).collect();
        match holders.as_slice() {
            [only] => Ok(only.id.clone()),
            [] => anyhow::bail!(
                "no agent with role '{input}'; known roles: {}",
                agents
                    .iter()
                    .map(|a| a.role.as_str())
                    .filter(|r| !r.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            many => anyhow::bail!(
                "role '{input}' is held by {} agents; use a uuid: {}",
                many.len(),
                many.iter()
                    .map(|a| a.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
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

    /// List the active profile config as raw JSON (sets and the default-set
    /// marker). The CLI renders the set names from it.
    pub async fn get_profiles(&self) -> Result<serde_json::Value> {
        self.handle_response(
            self.with_auth(self.inner.http.get(self.url("/profiles")))
                .send()
                .await
                .context("failed to connect to tagma")?,
            "failed to parse response",
        )
        .await
    }

    /// Rebind the agent to a named profile set (exact name; unknown names
    /// list the available sets). Caller must be the operator or a superior of
    /// the target. A live agent swaps its failover chain on the next wake-up.
    pub async fn bind_profile_set(
        &self,
        id: &AgentId,
        body: ProfileSetUpdateRequest,
    ) -> Result<AgentSummary> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .put(self.url(&format!("/agents/{id}/profile-set")))
                    .json(&body),
            )
            .send()
            .await
            .context("failed to connect to tagma")?,
            "failed to parse response",
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> TagmaClient {
        TagmaClient::builder(&server.uri()).build().unwrap()
    }

    async fn mount_agents(server: &MockServer, agents: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/agents"))
            .respond_with(ResponseTemplate::new(200).set_body_json(agents))
            .mount(server)
            .await;
    }

    fn summary(id: &str, role: Option<&str>) -> serde_json::Value {
        let mut v = serde_json::json!({
            "id": id,
            "workspace_root": "/tmp/ws",
            "state": "idle",
            "created_by": null,
        });
        if let Some(role) = role {
            v["role"] = serde_json::json!(role);
        }
        v
    }

    fn uuid(n: u16) -> String {
        format!("0b6c95a4-0000-4000-8000-{n:012x}")
    }

    #[tokio::test]
    async fn resolve_agent_ref_passes_uuid_verbatim_without_listing() {
        let server = MockServer::start().await;
        // No /agents mock is mounted: the uuid path must return before any
        // request, so a listing attempt would fail on the empty mock server.
        let client = client_for(&server);
        let id = client.resolve_agent_ref(&uuid(1)).await.unwrap();
        assert_eq!(id.to_string(), uuid(1));
    }

    #[tokio::test]
    async fn resolve_agent_ref_matches_role_exactly() {
        let server = MockServer::start().await;
        mount_agents(
            &server,
            serde_json::json!({"agents": [
                summary(&uuid(1), Some("lead-dev")),
                summary(&uuid(2), Some("reviewer-quality")),
            ]}),
        )
        .await;
        let client = client_for(&server);
        let id = client.resolve_agent_ref("reviewer-quality").await.unwrap();
        assert_eq!(id.to_string(), uuid(2));
    }

    #[tokio::test]
    async fn resolve_agent_ref_lists_known_roles_on_miss() {
        let server = MockServer::start().await;
        mount_agents(
            &server,
            serde_json::json!({"agents": [
                summary(&uuid(1), Some("lead-dev")),
                summary(&uuid(2), Some("reviewer-quality")),
                summary(&uuid(3), None),
            ]}),
        )
        .await;
        let client = client_for(&server);
        // Exact match is case-sensitive: the wrong case is a miss.
        let err = client.resolve_agent_ref("Lead-Dev").await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no agent with role 'Lead-Dev'"), "{msg}");
        assert!(msg.contains("lead-dev, reviewer-quality"), "{msg}");
    }

    #[tokio::test]
    async fn resolve_agent_ref_lists_uuids_when_role_is_ambiguous() {
        let server = MockServer::start().await;
        mount_agents(
            &server,
            serde_json::json!({"agents": [
                summary(&uuid(1), Some("scout")),
                summary(&uuid(2), Some("scout")),
            ]}),
        )
        .await;
        let client = client_for(&server);
        let err = client.resolve_agent_ref("scout").await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("held by 2 agents"), "{msg}");
        assert!(msg.contains(&uuid(1)), "{msg}");
        assert!(msg.contains(&uuid(2)), "{msg}");
    }

    #[tokio::test]
    async fn post_message_sends_defer_flag_and_parses_delivery_mode() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/agents/{}/message", uuid(1))))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "queue_depth": 0,
                "delivery_mode": "deferred",
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let id: AgentId = uuid(1).parse().unwrap();
        let resp = client.post_message(&id, "later", true).await.unwrap();
        assert_eq!(
            resp.delivery_mode,
            Some(kallip_common::protocol::DeliveryMode::Deferred)
        );

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["text"], "later");
        assert_eq!(body["defer"], true);
        assert!(
            body.get("attachment").is_none(),
            "a defer send without attachment keeps the minimal body"
        );
    }
}
