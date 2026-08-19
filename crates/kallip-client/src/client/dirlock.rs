//! Directory write-lock methods for [`TagmaClient`].
//!
//! Acquire/release/status/who for the workspace dirlock — plus the two
//! acquire/who response types, re-exported by `client.rs`. Split from
//! `client.rs` verbatim; the client core stays in the parent module.

use super::TagmaClient;
use anyhow::{Context, Result};
use kallip_common::agentid::AgentId;

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

impl TagmaClient {
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
}
