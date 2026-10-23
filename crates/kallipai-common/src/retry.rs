//! Retry record type for persistence and reporting.

use serde::{Deserialize, Serialize};

/// How many retry records the store keeps. The status endpoint renders the
/// most recent 20, so the bound is what the reporting surface shows; retry
/// budgets are counted in memory and never read this log.
pub const RETRY_LOG_KEEP: usize = 20;

/// Coarse failure class of a retry observation, decided by the runtime at record time
/// from the HTTP status (or transport nature) of the failed attempt. The
/// rendered provider error body stays in `error` for archival; `kind` is
/// what status output groups and summarizes on, so vendor wording never
/// leaks into the summary line.
/// A kind is recorded for the trigger, not for a performed retry: a class that
/// advances the chain instead of retrying records its kind with no wait.
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

/// One retry observation: the failure class the runtime met, the send attempt it
/// was charged to, and the wait a further attempt would take. A record does
/// not imply that a further send happened — a class that advances the chain
/// instead of retrying is recorded with `delay_secs: 0.0` and no next attempt (see [`RetryKind`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryRecord {
    /// Unix epoch seconds when the failure was observed.
    pub timestamp: u64,
    /// Which tool round the observation belongs to.
    pub round: usize,
    /// Send attempt this observation was charged to (1-based).
    pub attempt: u32,
    /// Maximum retry attempts configured.
    pub max_attempts: u32,
    /// Short description of the error that triggered this record.
    pub error: String,
    /// Seconds a further attempt would wait. `0.0` means there is no next
    /// attempt: the failure class advanced the chain instead of retrying, so the record
    /// is an observation rather than a retry that ran.
    pub delay_secs: f64,
    /// Provider id this retry was against (`None` on legacy records). Each record names the
    /// provider it was observed on; the retry budget itself belongs to the episode, not to
    /// the provider.
    #[serde(default)]
    pub provider: Option<String>,
    /// Coarse failure class (see [`RetryKind`]); defaults to transport on
    /// legacy records.
    #[serde(default)]
    pub kind: RetryKind,
    /// When the rate-limit window is expected to reopen, as epoch seconds
    /// (`now +` the reset hint the response body carried at record time). `None`
    /// when the failure carried no reset hint.
    ///
    /// The upstream's own value, as it gave it: this record is the offline
    /// aggregation face (`reset − first rejection`), so the trust bound the
    /// display surfaces apply to a fact's hint does not apply here.
    #[serde(default)]
    pub quota_reset: Option<u64>,
}

/// The send attempt a retry record is charged to: the three values every record
/// carries, named once so a call site cannot swap them.
#[derive(Debug, Clone, Copy)]
pub struct RetryAttempt {
    /// Which tool round the observation belongs to.
    pub round: usize,
    /// Send attempt this observation was charged to (1-based).
    pub attempt: u32,
    /// Maximum retry attempts configured.
    pub max_attempts: u32,
}

impl RetryRecord {
    /// A retry that ran: `delay_secs` is the wait its sender took, and no
    /// rate-limit window is claimed — a rejection carrying a reset hint
    /// advances the chain instead of retrying, so it is an [`Self::observed`]
    /// record.
    pub fn retry(
        at: RetryAttempt,
        now: u64,
        provider: Option<String>,
        kind: RetryKind,
        error: String,
        delay_secs: f64,
    ) -> Self {
        Self {
            timestamp: now,
            round: at.round,
            attempt: at.attempt,
            max_attempts: at.max_attempts,
            error,
            delay_secs,
            provider,
            kind,
            quota_reset: None,
        }
    }

    /// A failure observed without a wait: the class advanced the chain rather
    /// than retrying, so `delay_secs` is zero and `quota_reset` carries the
    /// upstream's own reset hint when the response gave one.
    pub fn observed(
        at: RetryAttempt,
        now: u64,
        provider: Option<String>,
        kind: RetryKind,
        error: String,
        quota_reset: Option<u64>,
    ) -> Self {
        Self {
            timestamp: now,
            round: at.round,
            attempt: at.attempt,
            max_attempts: at.max_attempts,
            error,
            delay_secs: 0.0,
            provider,
            kind,
            quota_reset,
        }
    }
}
