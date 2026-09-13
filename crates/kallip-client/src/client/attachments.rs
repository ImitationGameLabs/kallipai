//! Attachment faces for [`TagmaClient`].

//! `attachment_ingest` is the record form: the tagma fetches the bytes
//! from the files service. `attachment_store` is the path form: the
//! bytes ride the request body and land in the tagma's local blob
//! store as the master copy. `attachment_store_blob` re-ingests an
//! already-stored blob by its content address. All faces are
//! self-scoped server-side (the operator token may target any agent);
//! the tagma enforces the bound set's modalities before recording the
//! turn (live store plus history sidecar).

use super::TagmaClient;
use anyhow::{Context, Result};
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{
    AttachmentIngestLocalResponse, AttachmentIngestRequest, AttachmentIngestResponse,
};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
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

    /// Store a local image through the path form: the bytes ride the
    /// request body, the media type is `Content-Type`, and the file
    /// name goes percent-encoded (RFC 3986) in `X-Kallip-File-Name`
    /// for tracing. Returns the recorded turn id plus the blob id of
    /// the stored master copy.
    pub async fn attachment_store(
        &self,
        id: &AgentId,
        bytes: Vec<u8>,
        media_type: &str,
        file_name: &str,
        caption: Option<&str>,
    ) -> Result<AttachmentIngestLocalResponse> {
        let encoded = utf8_percent_encode(file_name, NON_ALPHANUMERIC).to_string();
        let mut request = self
            .inner
            .http
            .post(self.url(&format!("/agents/{id}/attachments")))
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .header("x-kallip-file-name", encoded)
            .body(bytes);
        if let Some(caption) = caption {
            request = request.query(&[("caption", caption)]);
        }
        self.handle_response(
            self.with_auth(request)
                .send()
                .await
                .context("failed to connect to tagma")?,
            "failed to parse the store response",
        )
        .await
    }

    /// Re-ingest an already-stored blob through the reference variant:
    /// the request carries `X-Kallip-Blob-Id` and no body; the tagma
    /// reads the bytes from its own attachment store. The media type
    /// is presentation metadata on the new reference.
    pub async fn attachment_store_blob(
        &self,
        id: &AgentId,
        blob_id: &str,
        media_type: &str,
        caption: Option<&str>,
    ) -> Result<AttachmentIngestLocalResponse> {
        let mut request = self
            .inner
            .http
            .post(self.url(&format!("/agents/{id}/attachments")))
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .header("x-kallip-blob-id", blob_id);
        if let Some(caption) = caption {
            request = request.query(&[("caption", caption)]);
        }
        self.handle_response(
            self.with_auth(request)
                .send()
                .await
                .context("failed to connect to tagma")?,
            "failed to parse the store response",
        )
        .await
    }
}
