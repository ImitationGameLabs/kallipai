//! Retry record type for persistence and reporting.

use serde::{Deserialize, Serialize};

/// How many retry records the store keeps. The status endpoint renders the
/// most recent 20 and the per-endpoint retry budget counts in-window
/// records, so the bound only bites far past any configured budget.
pub const RETRY_LOG_KEEP: usize = 20;

/// Coarse failure class for a retry, decided by the runtime at record time
/// from the HTTP status (or transport nature) of the failed attempt. The
/// rendered provider error body stays in `error` for archival; `kind` is
/// what status output groups and summarizes on, so vendor wording never
/// leaks into the summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetryKind {
    /// HTTP 429 — quota exhaustion, usually carries a reset hint.
    RateLimit,
    /// HTTP 408 or a client-side request deadline.
    Timeout,
    /// HTTP 5xx — the provider is unhealthy but reachable.
    Server,
    /// Connection/stream failure below the HTTP layer (DNS, TLS, mid-stream drop).
    Transport,
}

impl Default for RetryKind {
    /// Legacy records (persisted before `kind` existed) classify as
    /// transport: the pre-`kind` retry paths recorded send and stream drops.
    fn default() -> Self {
        Self::Transport
    }
}

impl RetryKind {
    /// Stable rendering key, shared by the CLI summary lines so
    /// `last: rate-limit` reads identically everywhere.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate-limit",
            Self::Timeout => "timeout",
            Self::Server => "server",
            Self::Transport => "transport",
        }
    }
}

/// Persistent record of a single retry attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryRecord {
    /// Unix epoch seconds when this retry was triggered.
    pub timestamp: u64,
    /// Which tool round the retry belongs to.
    pub round: usize,
    /// Retry attempt number (1-based).
    pub attempt: u32,
    /// Maximum retry attempts configured.
    pub max_attempts: u32,
    /// Short description of the error that triggered this retry.
    pub error: String,
    /// Backoff delay in seconds before the next attempt.
    pub delay_secs: f64,
    /// Provider id this retry was against (`None` on legacy records). Scopes the per-provider
    /// retry budget during within-set failover: the budget is provider-keyed because rate
    /// limits are provider-scoped (two profiles sharing one provider share one budget).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Coarse failure class (see [`RetryKind`]); defaults to transport on
    /// legacy records.
    #[serde(default)]
    pub kind: RetryKind,
    /// When the rate-limit window is expected to reopen, as epoch seconds
    /// (`now + retry-after` at record time). `None` when the failure
    /// carried no reset hint.
    #[serde(default)]
    pub quota_reset: Option<u64>,
}
