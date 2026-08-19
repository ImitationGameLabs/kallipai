//! Inbox methods for [`TagmaClient`].
//!
//! List, read, summarize, mark-done, and clear an agent's inbox. Split
//! from `client.rs` verbatim; the client core stays in the parent module.

use super::TagmaClient;
use anyhow::{Context, Result};
use kallip_common::agentid::AgentId;

impl TagmaClient {
    /// List messages in an agent's inbox (`GET /agents/{id}/inbox`).
    pub async fn inbox_list(
        &self,
        id: &AgentId,
        status: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<crate::InboxEntry>> {
        let mut req = self
            .inner
            .http
            .get(self.url(&format!("/agents/{id}/inbox")));
        if let Some(s) = status {
            req = req.query(&[("status", s)]);
        }
        if let Some(n) = limit {
            req = req.query(&[("limit", n)]);
        }
        self.handle_response(
            self.with_auth(req)
                .send()
                .await
                .context("failed to list inbox")?,
            "failed to parse inbox list response",
        )
        .await
    }

    /// Read a single inbox message, marking it as read
    /// (`GET /agents/{id}/inbox/{msg_id}`).
    pub async fn inbox_read(&self, id: &AgentId, msg_id: i64) -> Result<crate::InboxEntry> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/inbox/{msg_id}"))),
            )
            .send()
            .await
            .context("failed to read inbox message")?,
            "failed to parse inbox entry",
        )
        .await
    }

    /// Get inbox summary counts (`GET /agents/{id}/inbox/summary`).
    pub async fn inbox_summary(&self, id: &AgentId) -> Result<crate::InboxSummary> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/inbox/summary"))),
            )
            .send()
            .await
            .context("failed to get inbox summary")?,
            "failed to parse inbox summary",
        )
        .await
    }

    /// Mark an inbox message as done (`PUT /agents/{id}/inbox/{msg_id}`).
    pub async fn inbox_mark_done(&self, id: &AgentId, msg_id: i64) -> Result<()> {
        self.ensure_success(
            self.with_auth(
                self.inner
                    .http
                    .put(self.url(&format!("/agents/{id}/inbox/{msg_id}"))),
            )
            .send()
            .await
            .context("failed to mark inbox message done")?,
        )
        .await?;
        Ok(())
    }

    /// Clear inbox messages (`DELETE /agents/{id}/inbox`).
    /// When `all` is false (default), only done messages are removed.
    pub async fn inbox_clear(&self, id: &AgentId, all: bool) -> Result<usize> {
        #[derive(serde::Deserialize)]
        struct ClearResponse {
            cleared: usize,
        }
        let mut req = self
            .inner
            .http
            .delete(self.url(&format!("/agents/{id}/inbox")));
        if all {
            req = req.query(&[("all", "true")]);
        }
        let resp: ClearResponse = self
            .handle_response(
                self.with_auth(req)
                    .send()
                    .await
                    .context("failed to clear inbox")?,
                "failed to parse clear response",
            )
            .await?;
        Ok(resp.cleared)
    }
}
