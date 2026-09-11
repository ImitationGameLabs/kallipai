//! Attachment-ingest method for [`TagmaClient`].
//!
//! The `kallip image read` face: posts the ingest request against the
//! caller's own agent context (self-scoped server-side; the operator token
//! also works for targeting any agent). The tagma enforces the bound set's
//! modalities, fetches the bytes from the files service, and records the
//! turn (live store plus history sidecar).

use super::TagmaClient;
use anyhow::{Context, Result};
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{AttachmentIngestRequest, AttachmentIngestResponse};

impl TagmaClient {
    /// Ingest an attachment into the agent's live context. Returns the
    /// recorded turn id; an `ApiError` (403) when the bound set cannot
    /// serve the requested modality.
    pub async fn attachment_ingest(
        &self,
        id: &AgentId,
        req: &AttachmentIngestRequest,
    ) -> Result<AttachmentIngestResponse> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/agents/{id}/attachments/ingest")))
                    .json(req),
            )
            .send()
            .await
            .context("failed to connect to tagma")?,
            "failed to parse ingest response",
        )
        .await
    }
}
