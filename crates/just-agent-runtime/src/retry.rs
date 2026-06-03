//! LLM request retry with exponential backoff.
//!
//! Retries transient failures (connection errors, HTTP 429/5xx) at the
//! `stream_chat_completion` boundary. Once content deltas start flowing,
//! retry is off the table — mid-stream failures propagate as errors.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::error::Error as StdError;

use anyhow::Result;
use just_llm_client::{LlmError, TransportError};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::event::AgentEvent;
use just_agent_common::retry::RetryRecord;

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
            max_retries: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            retry_timeout: Duration::from_secs(120),
        }
    }
}

enum RetryDecision {
    Retryable,
    Permanent,
}

/// Walk the error source chain looking for a [`TransportError`].
fn find_transport_error<'a>(error: &'a (dyn StdError + 'static)) -> Option<&'a TransportError> {
    let mut current: Option<&'a (dyn StdError + 'static)> = Some(error);
    while let Some(err) = current {
        if let Some(te) = err.downcast_ref::<TransportError>() {
            return Some(te);
        }
        current = err.source();
    }
    None
}

/// Classify an LLM error as retryable or permanent.
fn classify(error: &LlmError) -> RetryDecision {
    match error {
        LlmError::InvalidRequest(_)
        | LlmError::UnsupportedCapability { .. }
        | LlmError::UnimplementedCapability { .. }
        | LlmError::UnavailableCapability { .. } => RetryDecision::Permanent,
        LlmError::Backend { source, .. } => {
            let Some(te) = find_transport_error(source.as_ref()) else {
                return RetryDecision::Permanent;
            };
            classify_transport_error(te)
        }
    }
}

fn classify_transport_error(te: &TransportError) -> RetryDecision {
    match te {
        TransportError::Transport(reqwest_err) => {
            if reqwest_err.is_connect() || reqwest_err.is_timeout() || reqwest_err.is_body() {
                RetryDecision::Retryable
            } else {
                RetryDecision::Permanent
            }
        }
        TransportError::HttpStatus { status, .. } => {
            let code = status.as_u16();
            if code == 429 || (500..=599).contains(&code) {
                RetryDecision::Retryable
            } else {
                RetryDecision::Permanent
            }
        }
        _ => RetryDecision::Permanent,
    }
}

/// Compute backoff delay for the given attempt with simple jitter.
fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let exp_delay = policy
        .base_delay
        .saturating_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX));
    let capped = exp_delay.min(policy.max_delay);

    // Simple time-based jitter: mix in subsecond precision to avoid
    // synchronized retry storms across agent instances.
    let jitter_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    let jitter = Duration::from_nanos(jitter_nanos % policy.base_delay.as_nanos() as u64);

    capped + jitter
}

#[allow(clippy::too_many_arguments)]
/// Open a streaming chat completion with retry on transient errors.
///
/// Uses a global retry budget shared across the agent's lifetime: `prior_retries`
/// is the number of recent retries (within `retry_timeout` window) already consumed.
/// Remaining budget = `max_retries - prior_retries`.
///
/// On each retry attempt, appends a [`RetryRecord`] to `retry_log` and emits
/// an [`AgentEvent::Retrying`] via `event_tx`. Returns the stream on success.
///
/// The caller is responsible for merging `retry_log` into persistent storage
/// (e.g., `ContextStore::retry_log`) and calling `persist()`.
pub async fn stream_with_retry(
    client: &just_llm_client::ChatClient,
    request: just_llm_client::types::chat::ChatCompletionRequest,
    policy: &RetryPolicy,
    round: usize,
    event_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    retry_log: &mut Vec<RetryRecord>,
    prior_retries: u32,
    cancel: CancellationToken,
) -> Result<just_llm_client::ChatCompletionStream> {
    if policy.max_retries == 0 {
        return Ok(client.stream_chat_completion(request).await?);
    }

    let remaining = policy.max_retries.saturating_sub(prior_retries);
    if remaining == 0 {
        return Ok(client.stream_chat_completion(request).await?);
    }

    let max_attempts = remaining + 1;
    let deadline = tokio::time::Instant::now() + policy.retry_timeout;
    let mut last_error: Option<LlmError> = None;

    for attempt in 1..=max_attempts {
        match client.stream_chat_completion(request.clone()).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                let is_retryable = matches!(classify(&e), RetryDecision::Retryable);
                let remaining_attempts = max_attempts - attempt;

                if !is_retryable || remaining_attempts == 0 {
                    return Err(e.into());
                }

                let delay = backoff_delay(policy, attempt - 1);
                let delay_secs = delay.as_secs_f64();
                let error_msg = format!("{e:#}");
                let global_attempt = prior_retries + attempt;

                info!(
                    attempt = global_attempt,
                    max_attempts = policy.max_retries,
                    delay_secs,
                    "LLM request failed, retrying"
                );

                let record = RetryRecord {
                    timestamp: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    round,
                    attempt: global_attempt,
                    max_attempts: policy.max_retries,
                    error: error_msg.clone(),
                    delay_secs,
                };
                retry_log.push(record);

                event_tx
                    .send(AgentEvent::Retrying {
                        attempt: global_attempt,
                        max_attempts: policy.max_retries,
                        error: error_msg,
                        delay_secs,
                    })
                    .await
                    .ok();

                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(e.into());
                }

                tokio::select! {
                    _ = tokio::time::sleep(delay.min(remaining)) => {}
                    _ = cancel.cancelled() => {
                        return Err(anyhow::anyhow!("cancelled during retry backoff"));
                    }
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("all retry attempts exhausted")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_delay_increases_exponentially() {
        let policy = RetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            retry_timeout: Duration::from_secs(120),
        };

        let d0 = backoff_delay(&policy, 0);
        let d1 = backoff_delay(&policy, 1);
        let d2 = backoff_delay(&policy, 2);

        // Rough exponential growth (jitter makes exact comparison impossible).
        assert!(d0 >= Duration::from_secs(1));
        assert!(d1 >= Duration::from_secs(2));
        assert!(d2 >= Duration::from_secs(4));
    }

    #[test]
    fn backoff_delay_capped_at_max() {
        let policy = RetryPolicy {
            max_retries: 10,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
            retry_timeout: Duration::from_secs(120),
        };

        let d = backoff_delay(&policy, 100);
        // 1 * 2^100 would overflow; cap applies.
        assert!(d <= Duration::from_secs(5) + Duration::from_secs(1));
    }
}
