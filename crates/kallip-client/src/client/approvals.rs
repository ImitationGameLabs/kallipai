//! Approval methods for [`TagmaClient`].
//!
//! Decide, list, and fetch pending approvals — the approvals resource
//! domain. Split from `client.rs` verbatim; the client core stays in the
//! parent module.

use super::TagmaClient;
use crate::types::ListApprovalsParams;
use anyhow::{Context, Result};
use kallip_common::protocol::{ApprovalDecisionBody, ApprovalEntry, ListApprovalsResponse};

impl TagmaClient {
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
            .context("failed to connect to tagma")?,
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
                .context("failed to connect to tagma")?,
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
                .context("failed to connect to tagma")?,
            "failed to parse response",
        )
        .await
    }
}
