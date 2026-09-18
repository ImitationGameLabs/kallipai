//! The forwarding path's support: the quota gate, the rate-limit /
//! budget error envelopes, and the audit-riding response shapes.
//!
//! The matrix gate runs after authorization and before the upstream call.
//! fail-closed throughout: a window denial is a 429 in the provider's
//! error envelope, and a check that cannot run (a database failure behind
//! the budget sums) denies with a 5xx -- never a silent pass-through.
//!
//! Response side: a streamed upstream is forwarded through a generator
//! that scans for the usage block in passing and lands the audit row when
//! the stream ends; a buffered upstream is parsed before it goes out and
//! lands the row synchronously. Either way a failed audit write is logged
//! and swallowed (see the audit module doc).

use std::sync::Arc;
use std::time::Instant;

use async_stream::try_stream;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;

use crate::audit::{self, RequestAuditRow, SseUsageScanner};
use crate::db::Db;
use crate::quota::{Denial, QuotaLedger};
use crate::registry::profile::Model as Profile;
use crate::secret::ResolvedKey;

/// Buffered-response cap for the non-streaming branch. A non-streaming
/// response is a complete JSON document by contract; past this bound it is
/// treated as unbounded and turned into a 502 rather than silently
/// forwarded without its usage/audit side.
pub(crate) const MAX_RESPONSE_BUFFER: usize = 32 * 1024 * 1024;

/// Run the full matrix gate: rpm (pre-check + increment), tpm (window
/// check; tokens refill after the response), then the two budget checks
/// against the audit table's cost sums. Budget numbers are BIGINT
/// micro-units; nothing prices rows yet (cost_micros is always NULL), so
/// the sums are 0 and the checks cannot trigger -- the path is real, the
/// input arrives with the pricing face.
pub(crate) async fn enforce_quotas(
    quota: &Arc<QuotaLedger>,
    db: &Db,
    resolved: &ResolvedKey,
    profile: &Profile,
) -> Result<(), Box<Response>> {
    let share = &resolved.tagma_quota;
    quota
        .check_rpm(
            &resolved.tagma_id,
            share.rpm_limit,
            &profile.profile_id,
            profile.rpm_limit,
        )
        .map_err(|d| Box::new(denied_response(d)))?;
    quota
        .check_tpm(
            &resolved.tagma_id,
            share.tpm_limit,
            &profile.profile_id,
            profile.tpm_limit,
        )
        .map_err(|d| Box::new(denied_response(d)))?;

    if let Some(limit) = share.max_budget {
        let spent = budget_spent(db, audit::SpendBy::Tagma(resolved.tagma_id.clone())).await?;
        if spent >= limit {
            return Err(Box::new(budget_response("tagma", limit)));
        }
    }
    if let Some(limit) = profile.max_budget {
        let spent = budget_spent(db, audit::SpendBy::Profile(profile.profile_id.clone())).await?;
        if spent >= limit {
            return Err(Box::new(budget_response("profile", limit)));
        }
    }
    Ok(())
}

/// The budget sum query; a failure denies (fail-closed) with a 500.
async fn budget_spent(db: &Db, by: audit::SpendBy) -> Result<i64, Box<Response>> {
    audit::spent_cost_micros(db, by).await.map_err(|e| {
        tracing::warn!(error = %e, "budget check failed; denying");
        Box::new(quota_unavailable())
    })
}

/// The audit fields the forwarding path has gathered for one request.
pub(crate) struct PendingAudit {
    pub db: Db,
    pub row: RequestAuditRow,
}

impl PendingAudit {
    /// Best-effort landing of the row (log and swallow on failure).
    pub(crate) async fn land(self) {
        if let Err(e) = audit::record_request(&self.db, self.row).await {
            tracing::warn!(error = %e, "request audit write failed");
        }
    }
}

/// Refill the tpm windows with the response's token counts (no-op when the
/// upstream carried no usage block).
pub(crate) fn refill_tokens(
    quota: &Arc<QuotaLedger>,
    tagma_id: &str,
    profile_id: &str,
    usage: Option<audit::ExtractedUsage>,
) {
    if let Some(u) = usage {
        quota.record_tokens(tagma_id, profile_id, u.total_tokens.max(0) as u64);
    }
}

/// A 429 for a window denial, in the provider's error envelope: this
/// face's consumers are LLM clients reading the OpenAI wire vocabulary
/// (the same reasoning as the 502 hand-build in the forwarding face).
pub(crate) fn denied_response(denial: Denial) -> Response {
    let message = format!(
        "rate limit exceeded: {} {} limit is {}",
        denial.dimension, denial.kind, denial.limit
    );
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "error": {
                "message": message,
                "type": "rate_limit_exceeded",
                "code": "rate_limit_exceeded"
            }
        })),
    )
        .into_response()
}

/// A 429 for an exhausted budget (OpenAI's `insufficient_quota` type).
pub(crate) fn budget_response(dimension: &str, limit: i64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "error": {
                "message": format!("budget exhausted: the {dimension} max_budget of {limit} micro-units is spent"),
                "type": "insufficient_quota",
                "code": "insufficient_quota"
            }
        })),
    )
        .into_response()
}

/// A 500 for a quota check that cannot run (fail-closed posture).
pub(crate) fn quota_unavailable() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": {
                "message": "quota check unavailable",
                "type": "quota_check_failed",
                "code": "internal_error"
            }
        })),
    )
        .into_response()
}

/// The streamed-response shape: forward every chunk verbatim, feed the
/// usage scanner in passing, land the audit row when the upstream stream
/// ends (spawned: the generator cannot await). A client that disconnects
/// mid-stream stops polling, so that row never lands -- an accepted
/// boundary, declared in the batch report.
pub(crate) fn audited_stream(
    upstream: impl futures_util::Stream<Item = reqwest::Result<axum::body::Bytes>>
    + Send
    + Unpin
    + 'static,
    pending: PendingAudit,
    quota: Arc<QuotaLedger>,
    tagma_id: String,
    profile_id: String,
    started: Instant,
) -> impl futures_util::Stream<Item = reqwest::Result<axum::body::Bytes>> {
    try_stream! {
        let mut upstream = upstream;
        let mut scanner = SseUsageScanner::new();
        while let Some(item) = upstream.next().await {
            match item {
                Ok(chunk) => {
                    scanner.feed(&chunk);
                    yield chunk;
                }
                Err(e) => Err(e)?,
            }
        }
        // Stream over: extract, refill, land the row -- all best-effort.
        let usage = scanner.finish();
        refill_tokens(&quota, &tagma_id, &profile_id, usage);
        let duration_ms = started.elapsed().as_millis() as i64;
        let mut row = pending.row;
        row.duration_ms = duration_ms;
        row.usage = usage;
        tokio::spawn(async move {
            PendingAudit { db: pending.db, row }.land().await;
        });
    }
}
