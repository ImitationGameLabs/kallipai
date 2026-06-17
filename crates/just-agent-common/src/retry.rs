//! Retry record type for persistence and reporting.

use serde::{Deserialize, Serialize};

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
    /// Endpoint id this retry was against (`None` on legacy records). Scopes the per-endpoint
    /// retry budget during within-tier failover: the budget is endpoint-keyed because rate
    /// limits are endpoint-scoped (two profiles sharing one endpoint share one budget).
    #[serde(default)]
    pub endpoint: Option<String>,
}
