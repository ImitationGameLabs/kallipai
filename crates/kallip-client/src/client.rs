use std::sync::Arc;

/// Result of a directory lock acquire attempt.
#[derive(Debug, serde::Deserialize)]
pub struct DirLockAcquireResponse {
    pub acquired: bool,
    pub already_held: bool,
    /// Present only when acquisition failed (busy) — the holder's agent id.
    pub holder: Option<String>,
}

/// Result of a "who holds this dir" query.
#[derive(Debug, serde::Deserialize)]
pub struct DirLockWhoResponse {
    pub holder: Option<String>,
}

use anyhow::{Context, Result};
use just_llm_client::JsonEventStream;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{ApiError, SseEvent};

use crate::types::{ListApprovalsParams, MessageRequest};
use crate::{
    AgentPermissionsResponse, AgentStatusResponse, AgentSummary, ApprovalDecisionBody,
    ApprovalEntry, CreateAgentRequest, CreateAgentResponse, ExecPolicy, ListAgentsResponse,
    ListApprovalsResponse, ListSkillPromoteRecordsResponse, PromoteDecision, SkillMeta,
    SkillPathsResponse, SkillPromoteDecisionBody, SkillPromoteShowResponse,
    SkillPromoteSubmitResponse, TokenBudgetResponse, TokenBudgetUpdateRequest,
    UpdateActivityRequest, UpdateAgentMetadataRequest,
};

struct Inner {
    base_url: String,
    http: reqwest::Client,
    auth_token: Option<String>,
}

/// Async client for the kallip daemon HTTP API.
#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<Inner>,
}

impl DaemonClient {
    /// Start building a [`DaemonClient`].
    ///
    /// `base_url` is required and is the daemon's HTTP root (e.g.
    /// `http://127.0.0.1:3000`).  Chain `.auth_token()` and/or
    /// `.http_client()` to override defaults, then call `.build()`.
    ///
    /// The default HTTP client is created lazily in [`DaemonClientBuilder::build`]
    /// so that callers who override it via `.http_client()` never pay the cost
    /// of constructing the default `reqwest::Client`.
    pub fn builder(base_url: &str) -> DaemonClientBuilder {
        DaemonClientBuilder {
            base_url: base_url.trim_end_matches('/').to_owned(),
            auth_token: None,
            http: None,
        }
    }

    /// Construct a client from environment variables.
    ///
    /// Reads `KALLIP_DAEMON_URL` (default: `http://127.0.0.1:3000`) and
    /// `KALLIP_AUTH_TOKEN` (required). Returns an error if the token is
    /// missing, with guidance tailored to common scenarios:
    ///
    /// - **Agent running inside the daemon**: the token is embedded
    ///   automatically in the spawned agent's environment, this should not happen.
    /// - **Operator user**: copy the token from the daemon startup output and
    ///   `export KALLIP_AUTH_TOKEN=<token>`.
    /// - **Automation**: set `KALLIP_AUTH_TOKEN` to the same value as the
    ///   daemon's `KALLIP_OPERATOR_TOKEN`.
    pub fn from_env() -> Result<Self> {
        let (url, token) = read_env_config()?;
        Self::builder(&url).auth_token(token).build()
    }

    /// Like [`from_env()`](Self::from_env), but injects a pre-built HTTP client.
    ///
    /// Use this when the caller needs to control TLS configuration (e.g.
    /// disabling cert verification for loopback-only connections in minimal
    /// containers).
    pub fn from_env_with_http(http: reqwest::Client) -> Result<Self> {
        let (url, token) = read_env_config()?;
        Self::builder(&url)
            .auth_token(token)
            .http_client(http)
            .build()
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

    // -- HTTP helpers ---------------------------------------------------------

    /// Send request, parse structured JSON error on non-2xx, deserialize
    /// success body as `T`.
    async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
        context_msg: &'static str,
    ) -> Result<T> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Envelope>(&body)
                .map(|e| e.error.message)
                .unwrap_or(body);
            return Err(ApiError {
                status: status.as_u16(),
                message,
            }
            .into());
        }
        response.json().await.context(context_msg)
    }

    /// Send request, parse structured JSON error on non-2xx, return raw
    /// response (for SSE streams that need the body as-is).
    async fn ensure_success(&self, response: reqwest::Response) -> Result<reqwest::Response> {
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Envelope>(&body)
                .map(|e| e.error.message)
                .unwrap_or(body);
            return Err(ApiError {
                status: status.as_u16(),
                message,
            }
            .into());
        }
        Ok(response)
    }

    // -- Agent lifecycle ------------------------------------------------------

    /// Spawn a new agent instance on the daemon.
    pub async fn spawn(&self, req: CreateAgentRequest) -> Result<AgentId> {
        let resp: CreateAgentResponse = self
            .handle_response(
                self.with_auth(self.inner.http.post(self.url("/agents")).json(&req))
                    .send()
                    .await
                    .context("failed to connect to daemon")?,
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

    /// List agent instances. Pass `created_by = Some(sup)` to list only a
    /// superior's direct subagents; `None` lists all agents.
    pub async fn list_agents(&self, created_by: Option<&AgentId>) -> Result<Vec<AgentSummary>> {
        let mut req = self.with_auth(self.inner.http.get(self.url("/agents")));
        if let Some(sup) = created_by {
            req = req.query(&[("created_by", sup.to_string())]);
        }
        let resp: ListAgentsResponse = self
            .handle_response(
                req.send().await.context("failed to connect to daemon")?,
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
            .context("failed to connect to daemon")?,
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
            .context("failed to connect to daemon")?,
        )
        .await?;
        Ok(())
    }

    /// Remove an agent instance.
    /// Requires superior-level auth if the daemon enforces it.
    pub async fn remove_agent(&self, id: &AgentId) -> Result<()> {
        self.ensure_success(
            self.with_auth(self.inner.http.delete(self.url(&format!("/agents/{id}"))))
                .send()
                .await
                .context("failed to connect to daemon")?,
        )
        .await?;
        Ok(())
    }

    /// Interrupt the current agent operation gracefully.
    /// Requires superior-level auth if the daemon enforces it.
    pub async fn interrupt_agent(&self, id: &AgentId) -> Result<()> {
        self.ensure_success(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/agents/{id}/interrupt"))),
            )
            .send()
            .await
            .context("failed to connect to daemon")?,
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

    /// Issue a raw HTTP request against the daemon and return the streaming
    /// response, uninterpreted. Used by the herald's HTTP tunnel to proxy
    /// arbitrary daemon routes transparently: the caller controls method/path/
    /// headers/body, this client contributes only the base URL and the operator
    /// auth. The response status is forwarded as-is (success is NOT enforced) so
    /// the tunneled caller sees the daemon's real status code.
    ///
    /// Callers that stream long-lived responses (the herald tunnel's
    /// `/agents/{id}/events`) must supply an `http_client` without a total
    /// timeout via the builder; the default client has none, but the no-timeout
    /// property is the caller's responsibility for streaming use.
    pub async fn proxy_request(
        &self,
        method: reqwest::Method,
        path: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
    ) -> Result<reqwest::Response> {
        let mut req = self.with_auth(self.inner.http.request(method, self.url(path)));
        for (name, value) in headers {
            req = req.header(name, value);
        }
        if let Some(bytes) = body {
            req = req.body(bytes.to_vec());
        }
        req.send().await.context("daemon proxy request failed")
    }

    // -- Approvals ------------------------------------------------------------

    /// Send a decision (approve/deny) for an approval.
    pub async fn respond_approval(
        &self,
        approval_id: &str,
        decision: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        self.ensure_success(
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
            .context("failed to connect to daemon")?,
        )
        .await?;
        Ok(())
    }

    /// List approvals with optional filtering and pagination.
    pub async fn list_approvals(
        &self,
        params: &ListApprovalsParams,
    ) -> Result<ListApprovalsResponse> {
        let req = self.inner.http.get(self.url("/approvals")).query(params);
        self.handle_response(
            self.with_auth(req)
                .send()
                .await
                .context("failed to connect to daemon")?,
            "failed to parse response",
        )
        .await
    }

    /// Get a single approval by id.
    pub async fn get_approval(&self, id: &str) -> Result<ApprovalEntry> {
        let req = self.inner.http.get(self.url(&format!("/approvals/{id}")));
        self.handle_response(
            self.with_auth(req)
                .send()
                .await
                .context("failed to connect to daemon")?,
            "failed to parse response",
        )
        .await
    }

    // -- Agent status / permissions / policy ----------------------------------

    /// Get agent status including context usage and retry history.
    pub async fn agent_status(&self, id: &AgentId) -> Result<AgentStatusResponse> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/status"))),
            )
            .send()
            .await
            .context("failed to get agent status")?,
            "failed to parse status response",
        )
        .await
    }

    /// Get agent permission profile and tool policy rules.
    pub async fn agent_permissions(&self, id: &AgentId) -> Result<AgentPermissionsResponse> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/permissions"))),
            )
            .send()
            .await
            .context("failed to get agent permissions")?,
            "failed to parse permissions response",
        )
        .await
    }

    /// Get the `bash_exec` command-policy overrides for an agent.
    pub async fn get_exec_policy(&self, id: &AgentId) -> Result<ExecPolicy> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/exec-policy"))),
            )
            .send()
            .await
            .context("failed to get agent exec policy")?,
            "failed to parse exec policy response",
        )
        .await
    }

    /// Update the `bash_exec` command-policy overrides for an agent.
    pub async fn update_exec_policy(&self, id: &AgentId, policy: &ExecPolicy) -> Result<()> {
        self.ensure_success(
            self.with_auth(
                self.inner
                    .http
                    .put(self.url(&format!("/agents/{id}/exec-policy")))
                    .json(policy),
            )
            .send()
            .await
            .context("failed to update agent exec policy")?,
        )
        .await?;
        Ok(())
    }

    // -- Directory write-locks -------------------------------------------------

    /// Acquire the write-lock on `path` for agent `id`. Bounded by `timeout_secs`
    /// (default server-side); on conflict the server returns the holder via an
    /// `ApiError` (conflict).
    pub async fn dirlock_acquire(
        &self,
        id: &AgentId,
        path: &str,
        timeout_secs: Option<u64>,
    ) -> Result<DirLockAcquireResponse> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/agents/{id}/dirlocks")))
                    .json(&serde_json::json!({ "path": path, "timeout_secs": timeout_secs })),
            )
            .send()
            .await
            .context("failed to acquire directory lock")?,
            "failed to parse acquire response",
        )
        .await
    }

    /// Release the write-lock on `path` for agent `id`. Idempotent.
    pub async fn dirlock_release(&self, id: &AgentId, path: &str) -> Result<()> {
        self.ensure_success(
            self.with_auth(
                self.inner
                    .http
                    .delete(self.url(&format!("/agents/{id}/dirlocks")))
                    .json(&serde_json::json!({ "path": path })),
            )
            .send()
            .await
            .context("failed to release directory lock")?,
        )
        .await?;
        Ok(())
    }

    /// List the canonical directories agent `id` currently holds write-locks on.
    pub async fn dirlock_status(&self, id: &AgentId) -> Result<Vec<String>> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/dirlocks"))),
            )
            .send()
            .await
            .context("failed to get directory lock status")?,
            "failed to parse lock status response",
        )
        .await
    }

    /// Who holds the write-lock on `dir`, if anyone.
    pub async fn dirlock_who(&self, dir: &str) -> Result<Option<String>> {
        let resp: DirLockWhoResponse = self
            .handle_response(
                self.with_auth(
                    self.inner
                        .http
                        .get(self.url("/dirlocks"))
                        .query(&[("dir", dir)]),
                )
                .send()
                .await
                .context("failed to query directory lock holder")?,
                "failed to parse lock holder response",
            )
            .await?;
        Ok(resp.holder)
    }

    // -- Skills ---------------------------------------------------------------

    /// Get skill directory paths for an agent (shared + local).
    pub async fn skill_paths(&self, id: &AgentId) -> Result<SkillPathsResponse> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/skills/paths"))),
            )
            .send()
            .await
            .context("failed to get skill paths")?,
            "failed to parse skill paths response",
        )
        .await
    }

    /// Get skill metadata (name + description) for a specific skill.
    ///
    /// The skill name is URL-encoded so that nested paths like
    /// `code/refactoring` survive as a single path segment.
    pub async fn skill_meta(&self, id: &AgentId, name: &str) -> Result<SkillMeta> {
        let encoded = name.replace('/', "%2F");
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/skills/{encoded}/meta"))),
            )
            .send()
            .await
            .context("failed to get skill meta")?,
            "failed to parse skill meta response",
        )
        .await
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
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/agents/{id}/skills/{encoded}/promote-request"))),
            )
            .send()
            .await
            .context("failed to submit promote request")?,
            "failed to parse promote submit response",
        )
        .await
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
        self.handle_response(
            self.with_auth(req)
                .send()
                .await
                .context("failed to list promote requests")?,
            "failed to parse promote list response",
        )
        .await
    }

    /// Show a promote request with full old/new content for diff review.
    pub async fn show_promote_request(&self, id: &str) -> Result<SkillPromoteShowResponse> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/skill-promote-requests/{id}"))),
            )
            .send()
            .await
            .context("failed to show promote request")?,
            "failed to parse promote show response",
        )
        .await
    }

    /// Approve or deny a promote request.
    pub async fn respond_promote_request(
        &self,
        id: &str,
        decision: PromoteDecision,
        reason: Option<&str>,
    ) -> Result<()> {
        self.ensure_success(
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
            .context("failed to respond to promote request")?,
        )
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Token budget
    // -----------------------------------------------------------------------

    /// Get the daemon-wide token budget status.
    pub async fn get_token_budget(&self) -> Result<TokenBudgetResponse> {
        self.handle_response(
            self.with_auth(self.inner.http.get(self.url("/budget")))
                .send()
                .await
                .context("failed to get token budget")?,
            "failed to parse budget response",
        )
        .await
    }

    /// Adjust the daemon-wide token budget by a signed delta.
    ///
    /// Positive delta increases, negative delta decreases.
    pub async fn adjust_token_budget(&self, delta: i64) -> Result<TokenBudgetResponse> {
        self.handle_response(
            self.with_auth(self.inner.http.post(self.url("/budget")).json(
                &TokenBudgetUpdateRequest {
                    set_remaining: None,
                    delta: Some(delta),
                },
            ))
            .send()
            .await
            .context("failed to adjust token budget")?,
            "failed to parse budget response",
        )
        .await
    }

    /// Set the remaining daemon-wide token budget.
    ///
    /// The daemon computes `new_total = consumed + value`. Use `value == 0`
    /// to pause all agents (remaining = 0 triggers immediate budget exceeded).
    pub async fn set_token_budget(&self, value: u64) -> Result<TokenBudgetResponse> {
        self.handle_response(
            self.with_auth(self.inner.http.post(self.url("/budget")).json(
                &TokenBudgetUpdateRequest {
                    set_remaining: Some(value),
                    delta: None,
                },
            ))
            .send()
            .await
            .context("failed to set token budget")?,
            "failed to parse budget response",
        )
        .await
    }
}

// -- Env helpers ---------------------------------------------------------------

/// Read `KALLIP_DAEMON_URL` and `KALLIP_AUTH_TOKEN` from the
/// environment.  Returns `(url, token)`.
fn read_env_config() -> Result<(String, String)> {
    let url = std::env::var("KALLIP_DAEMON_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let token = std::env::var("KALLIP_AUTH_TOKEN").context(concat!(
        "KALLIP_AUTH_TOKEN is not set.\n",
        "\n",
        "How to obtain the token:\n",
        "- Agent (inside daemon): token is embedded automatically, check daemon setup.\n",
        "- Operator user: copy from daemon startup output, then:\n",
        "    export KALLIP_AUTH_TOKEN=<token>\n",
        "- Automation: start the daemon with a preset operator token:\n",
        "    KALLIP_OPERATOR_TOKEN=<secret> kallip-daemon\n",
        "  then use the same value for the client:\n",
        "    KALLIP_AUTH_TOKEN=<secret> kallip <command>",
    ))?;
    Ok((url, token))
}

// -- Builder ------------------------------------------------------------------

/// Fluent builder for [`DaemonClient`].
///
/// Created via [`DaemonClient::builder`].  `base_url` is required (passed to
/// `builder()`); `auth_token` and `http_client` are optional with sensible
/// defaults.
pub struct DaemonClientBuilder {
    base_url: String,
    auth_token: Option<String>,
    http: Option<reqwest::Client>,
}

impl DaemonClientBuilder {
    /// Set the bearer token for authenticating with the daemon.
    pub fn auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    /// Override the default [`reqwest::Client`].
    ///
    /// Use this when you need custom TLS settings (e.g. disabling cert
    /// verification for loopback-only connections in minimal containers).
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.http = Some(client);
        self
    }

    /// Consume the builder and produce a [`DaemonClient`].
    ///
    /// If no custom HTTP client was provided via `.http_client()`, a default
    /// `reqwest::Client` is constructed here.  Construction can fail if the
    /// system CA store is missing; callers that need to avoid this should
    /// supply their own client via `.http_client()`.
    pub fn build(self) -> Result<DaemonClient> {
        let http = match self.http {
            Some(client) => client,
            None => reqwest::ClientBuilder::new().build()?,
        };
        Ok(DaemonClient {
            inner: Arc::new(Inner {
                base_url: self.base_url,
                http,
                auth_token: self.auth_token,
            }),
        })
    }
}

// -- Wire-format helpers for structured error deserialization ------------------

/// JSON envelope matching the daemon's error response: `{"error":{"message":"..."}}`.
#[derive(serde::Deserialize)]
struct Envelope {
    error: Body,
}

#[derive(serde::Deserialize)]
struct Body {
    message: String,
}
