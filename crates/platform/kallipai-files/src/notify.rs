//! The lesche event-push surface: a successful delivery notifies the
//! recipient's side through the lesche's internal surface, so their UI/agent
//! learns a file arrived without polling. Mirrors the FilesControlPlane
//! shape (base URL + shared secret bearer, a bounded timeout) and is
//! best-effort by design: a failed push only logs, because the file itself
//! is already safely stored in the recipient's space and can be discovered
//! via the listing surface.

use std::time::Duration;

use serde_json::json;

/// The push contract the send transaction depends on, so tests can record
/// pushes instead of speaking HTTP (the ControlPlane precedent).
#[async_trait::async_trait]
pub trait NotifyPusher: Send + Sync {
    /// Push one file-delivered event. Best-effort by contract: an
    /// implementation logs its own failures and never retracts a delivery.
    async fn file_delivered(
        &self,
        to_user: &str,
        record_id: uuid::Uuid,
        path: &str,
        from: &str,
        name: &str,
        size: u64,
    );
}

/// A reqwest-backed client for one configured lesche internal surface.
#[derive(Clone)]
pub struct LescheNotifyClient {
    base: String,
    token: String,
    http: reqwest::Client,
}

/// The per-call timeout for an internal push. Same 10s contract as the
/// control-plane client (a same-host tiny JSON call is a backstop, not
/// expected latency).
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);

impl LescheNotifyClient {
    /// Wire the client from config. `None` (an unset URL/token) disables the
    /// push entirely -- the safe posture for a files service whose lesche
    /// does not mount the internal surface.
    pub fn new(base: String, token: String) -> Option<Self> {
        if base.is_empty() || token.is_empty() {
            return None;
        }
        Some(Self {
            base: base.trim_end_matches('/').to_owned(),
            token,
            http: reqwest::Client::builder()
                .timeout(NOTIFY_TIMEOUT)
                .build()
                .ok()?,
        })
    }
}

#[async_trait::async_trait]
impl NotifyPusher for LescheNotifyClient {
    async fn file_delivered(
        &self,
        to_user: &str,
        record_id: uuid::Uuid,
        path: &str,
        from: &str,
        name: &str,
        size: u64,
    ) {
        let body = json!({
            "to_user": to_user,
            "record_id": record_id,
            "path": path,
            "from": from,
            "name": name,
            "size": size,
        });
        let url = format!("{}/internal/file-delivered", self.base);
        let result = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await;
        match result {
            Ok(resp) if !resp.status().is_success() => {
                tracing::warn!(status = %resp.status(), "file-delivered push rejected");
            }
            Err(e) => {
                tracing::warn!(error = %e, "file-delivered push failed");
            }
            _ => {}
        }
    }
}
