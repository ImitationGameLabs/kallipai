//! Status, verification, and policy methods for [`TagmaClient`].
//!
//! Agent status, verification state, effective permissions, and the exec
//! policy get/update pair. Split from `client.rs` verbatim; the client core
//! stays in the parent module.

use super::TagmaClient;
use anyhow::{Context, Result};
use kallip_common::agentid::AgentId;
use kallip_common::policy::ExecPolicy;
use kallip_common::protocol::{AgentPermissionsResponse, AgentStatusResponse};

impl TagmaClient {
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

    /// Verify the caller's bearer matches the named agent id: `Ok(())` on match
    /// (tagma returns `204`), propagates `ApiError` (`401`) on mismatch. Lets a
    /// trusted peer service (e.g. `kallip-cron`) confirm an `(agent_id, token)`
    /// pair a client presented, without that service holding the agent-token
    /// index itself. Carries its own short timeout — verification must not hang
    /// the caller on a wedged tagma (unlike SSE, this is a one-shot request).
    pub async fn verify_agent(&self, id: &AgentId) -> Result<()> {
        self.ensure_success(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/verify")))
                    .timeout(std::time::Duration::from_secs(5)),
            )
            .send()
            .await
            .context("failed to verify agent")?,
        )
        .await
        .map(drop)
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
}
