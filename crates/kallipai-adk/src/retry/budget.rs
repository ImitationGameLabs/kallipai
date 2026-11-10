//! What one episode is allowed to spend, and what a settled failure writes.
//!
//! The knobs (`max_retries`, the delays, `retry_timeout`) and the send count
//! they imply ([`max_sends`]); the two writes a settled failure makes — one
//! shape slot spent on the episode, one fact recorded on the shared table —
//! and the local error a caller bug produces when the budget is gone.
//!
//! The arithmetic these knobs feed lives in [`super::wait`]: this file says how
//! many sends are allowed and what each one costs, that one says how long the
//! wait before the next send is.

use std::time::Duration;

use just_llm_client::BackendError;

use kallipai_common::facts::ProfilePacing;

use super::decision;
use super::decision::Episode;

/// Configuration for LLM request retry behavior.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub retry_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 10,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            retry_timeout: Duration::from_secs(300),
        }
    }
}

/// Total sends one episode allows: the initial attempt plus `max_retries`.
pub(crate) fn max_sends(policy: &RetryPolicy) -> u32 {
    policy.max_retries.saturating_add(1)
}

/// Charge a shape-class failure to the episode: the key is spent so that a
/// repeat of the same shape reads as endpoint-level. Called where a retry is
/// about to happen, so a failure the budget turns into a chain advance spends nothing.
pub(crate) fn spend_shape_slot(episode: &mut Episode, error: &BackendError) {
    if let Some(key) = decision::shape_key(error) {
        *episode = episode.shape_failed(key);
    }
}

/// The error a spent retry budget produces. The acquisition loop pre-checks
/// the ledger and advances the chain, so reaching the retry path with no
/// sends left is a caller bug; both sites share this one wording.
pub(crate) fn spent_budget_error() -> BackendError {
    BackendError::invalid_request(
        "retry budget for this episode is spent before the request could be sent",
    )
}

/// Write the fact this failure leaves behind, if there is one to write.
///
/// The one place both shells hand a settled failure to the shared table, so a
/// failure records the same fact wherever it is met. Fail-open by contract: no
/// handle, or a class the table does not track, writes nothing. The record is
/// keyed by the profile the failing send ran on, never by its endpoint.
pub(crate) fn record_fact(
    pacing: Option<&dyn ProfilePacing>,
    profile_id: &str,
    class: decision::FailureClass,
    error: &BackendError,
    now: u64,
) {
    let Some(pacing) = pacing else { return };
    if let Some(block) = decision::fact_of(class, error, now) {
        pacing.record(profile_id, block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_policy_pins_max_retries_at_ten() {
        // Nail the documented default (docs/en/reference/environment-variables/, .env.example) so
        // the two code sites (here and config::defaults::DEFAULT_MAX_RETRIES) cannot drift from it
        // silently.
        // Timing fields stay un-pinned here: the full-field mirror test in config/tests.rs
        // guards those against two-sided drift.
        assert_eq!(RetryPolicy::default().max_retries, 10);
    }

    /// A spent retry budget is a caller bug, not an upstream verdict: it classifies
    /// as request-permanent — never transient, so no further send is attempted —
    /// and leaves no fact in the shared table, because the endpoint is not at fault.
    #[test]
    fn the_spent_budget_error_is_a_local_stop_not_an_upstream_fact() {
        let error = spent_budget_error();
        let class = decision::classify(&error, decision::Phase::Connect);
        assert_eq!(class, decision::FailureClass::RequestPermanent);
        assert!(
            decision::fact_of(class, &error, 1_000).is_none(),
            "a local stop marks no endpoint"
        );
    }
}
