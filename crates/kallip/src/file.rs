//! The files-service client behind `kallip file *`: a thin reqwest face
//! over `PUT|GET /v1/files`, `GET /v1/files/{id}` and
//! `POST /v1/files/{id}/send`. Credentials ride the spawn env
//! (`KALLIP_POLIS_URL` origin, deriving the /v1/files base, plus
//! `KALLIP_FILES_TOKEN` bearer) -- the enrollment-code channel shape, so
//! the secret reaches the agent's environment without any CLI flag
//! ever carrying it.

use std::path::Path;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Response of a successful upload (the server's `PutResponse`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutResponse {
    /// The server-minted record id (the addressability handle).
    pub record_id: Uuid,
    /// The content address; identical bytes deduplicate onto one blob.
    pub blob_id: String,
}

/// Response of a successful delivery (the server's `SendResponse`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResponse {
    /// The recipient's own record (a new id, same blob).
    pub record_id: Uuid,
    pub blob_id: String,
    /// Where the copy landed in the recipient's space.
    pub path: String,
}

/// One row of a listing (the server's `FileEntryView`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub id: Uuid,
    pub path: String,
    /// Blob size in bytes.
    pub size: i64,
    /// RFC 3339 timestamp of the record's creation.
    pub created_at: String,
}

/// The error body the API serves: an `ApiError` envelope
/// `{"error":{"message":"..."}}` — the CLI surfaces the service's
/// own `message` rather than a bare status code.
#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    error: ApiErrorMessage,
}

#[derive(Debug, Deserialize)]
struct ApiErrorMessage {
    message: String,
}

/// A reqwest-backed client for one principal's files credentials.
pub struct FilesClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl FilesClient {
    /// Read the configuration from the environment: `KALLIP_POLIS_URL`
    /// names the platform edge origin (required -- a files token is
    /// credentials, so there is no deployment to assume), and
    /// `KALLIP_FILES_TOKEN` carries the bearer.
    pub fn from_env() -> anyhow::Result<Self> {
        let origin = kallip_common::polis::polis_origin(std::env::var("KALLIP_POLIS_URL").ok())?;
        let base_url = format!("{origin}/v1/files");
        let token = std::env::var("KALLIP_FILES_TOKEN")
            .map_err(|_| anyhow::anyhow!("KALLIP_FILES_TOKEN env var not set"))?;
        Ok(Self::new(base_url, token))
    }

    /// Build a client from explicit credentials (the test entry point).
    pub fn new(base_url: String, token: String) -> Self {
        // A connect timeout bounds the dead-URL case; no request timeout --
        // large transfers are the point, and the server caps body sizes.
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client builds");
        Self {
            base_url,
            token,
            http,
        }
    }

    /// Upload a local file to a space path. The body streams from the file
    /// (the reader is wrapped, never buffered whole); the length is stated
    /// up front because the service enforces its cap on content-length.
    pub async fn put_file(&self, path: &str, file: &Path) -> anyhow::Result<PutResponse> {
        let metadata = tokio::fs::metadata(file)
            .await
            .map_err(|e| anyhow::anyhow!("cannot read {file:?}: {e}"))?;
        let handle = tokio::fs::File::open(file)
            .await
            .map_err(|e| anyhow::anyhow!("cannot open {file:?}: {e}"))?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(handle));
        let response = self
            .http
            .put(self.base_url.clone())
            .query(&[("path", path)])
            .header("authorization", format!("Bearer {}", self.token))
            .header(reqwest::header::CONTENT_LENGTH, metadata.len())
            .body(body)
            .send()
            .await?;
        let response = check(response).await?;
        Ok(response.json().await?)
    }

    /// Download a record's content (the full blob; ranges are a server-side
    /// facility the CLI does not exercise). The body arrives whole: the
    /// server's upload cap bounds the size, so a streaming sink would add
    /// plumbing without changing the memory story meaningfully.
    pub async fn get_file(&self, id: Uuid) -> anyhow::Result<Vec<u8>> {
        let response = self
            .http
            .get(format!("{}/{}", self.base_url, id))
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await?;
        let response = check(response).await?;
        Ok(response.bytes().await?.to_vec())
    }

    /// Deliver a record into another principal's inbox. Exactly one target
    /// must be given (the server rejects both/neither); policy (same-space,
    /// T->U refusal) stays server-side -- the CLI is a thin face.
    pub async fn send_file(
        &self,
        id: Uuid,
        to_user: Option<&str>,
        to_tagma: Option<&str>,
    ) -> anyhow::Result<SendResponse> {
        #[derive(serde::Serialize)]
        struct SendRequest<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            to_user: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            to_tagma: Option<&'a str>,
        }
        let response = self
            .http
            .post(format!("{}/{}/send", self.base_url, id))
            .header("authorization", format!("Bearer {}", self.token))
            .json(&SendRequest { to_user, to_tagma })
            .send()
            .await?;
        let response = check(response).await?;
        Ok(response.json().await?)
    }

    /// List records in a slice of the caller's space (`self` or `shared`),
    /// optionally narrowed by a relative prefix. The page cap is the
    /// server's; `limit` can only lower it.
    pub async fn list_files(
        &self,
        space: &str,
        prefix: Option<&str>,
        limit: Option<u64>,
    ) -> anyhow::Result<Vec<FileEntry>> {
        let mut query = vec![("space", space.to_owned())];
        if let Some(prefix) = prefix {
            query.push(("prefix", prefix.to_owned()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let response = self
            .http
            .get(self.base_url.clone())
            .query(&query)
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await?;
        let response = check(response).await?;
        Ok(response.json().await?)
    }
}

/// Map a non-success response to an error carrying the service's own
/// message; pass successes through unchanged.
async fn check(response: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<ApiErrorBody>(&body)
        .map(|parsed| parsed.error.message)
        .unwrap_or(body);
    anyhow::bail!("files service returned {status}: {message}");
}

#[cfg(test)]
mod tests {
    use super::ApiErrorBody;

    /// The API wraps every failure as `{"error":{"message"}}`; the
    /// parser must reach through the envelope or all errors degrade
    /// to raw JSON on the terminal.
    #[test]
    fn error_body_reaches_through_the_envelope() {
        let parsed: ApiErrorBody =
            serde_json::from_str(r#"{"error":{"message":"admin cannot list content"}}"#)
                .expect("nested error envelope parses");
        assert_eq!(parsed.error.message, "admin cannot list content");
    }
}
