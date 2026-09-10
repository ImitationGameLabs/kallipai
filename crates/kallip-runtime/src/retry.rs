//! LLM request retry with exponential backoff.
//!
//! Retries transient failures (network errors, HTTP 429/5xx) at the
//! `just_llm_client::Conversation` boundary: each attempt asks the conversation to open a
//! streaming generation and classifies the resulting [`BackendError`] — by HTTP status for
//! provider rejections, by variant otherwise; the server's `retry-after` header is not
//! reachable through the conversation, so backoff is the computed schedule only. Once content
//! deltas start flowing, retry is off the table — mid-stream failures propagate as errors.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use just_llm_client::{BackendError, Conversation, ConversationStream, provider_rejection};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::event::AgentEvent;
use kallip_common::retry::{RetryKind, RetryRecord};

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

/// Outcome of a single conversation-bound streaming attempt.
enum Attempt {
    /// 2xx — a live stream ready to hand off to the caller.
    Stream(ConversationStream),
    /// Transient failure (HTTP 429/5xx/408, or a network send failure). Worth retrying in-profile.
    Retry {
        error: BackendError,
        kind: RetryKind,
    },
    /// Provider/profile-level permanent failure (401/403/404). A different profile (different
    /// credentials / provider / model) may succeed, so the failover loop advances the chain.
    Failover(BackendError),
    /// Request-level permanent failure (400/422). Fails identically on every profile, so the
    /// failover loop does not advance — it errors the round.
    Fatal(BackendError),
}

/// Why a streaming request ultimately failed, surfaced to the failover loop. Transient retries
/// that exhaust the in-profile budget become [`RequestFailure::Failover`] (a different profile
/// may recover); request-level errors are [`RequestFailure::Fatal`]; a cancel during backoff is
/// [`RequestFailure::Cancelled`] (distinct from `Failover` so the caller short-circuits to a
/// cancelled round without advancing the failover chain).
#[derive(Debug, thiserror::Error)]
pub enum RequestFailure {
    /// The profile/endpoint cannot serve this request, but another profile might — advance.
    #[error("endpoint-level failure (failover candidate): {0}")]
    Failover(#[source] BackendError),
    /// The request itself is bad — fails on every profile; do not advance.
    #[error("request-level failure: {0}")]
    Fatal(#[source] BackendError),
    /// The round was cancelled during a retry backoff — short-circuit to a cancelled outcome; do
    /// not advance the failover chain.
    #[error("retry cancelled during backoff")]
    Cancelled,
}

/// Run one attempt: open a streaming generation through the conversation, then classify
/// the outcome.
///
/// The conversation owns the wire payload (full send vs `previous_response_id` delta), so
/// this layer sees only its verdict: [`BackendError::InvalidRequest`],
/// [`BackendError::Serialization`], and [`BackendError::Unserializable`] are request-shape
/// failures — fatal on every profile. A provider rejection is classified by HTTP status
/// (429/408/5xx retry, 401/403/404 fail over, other statuses fatal); a provider error with no
/// reachable HTTP status never reached the provider — transport-level, retried. The request is
/// re-cloned per attempt: `stream_generate` consumes it, and a failed attempt leaves the
/// conversation's anchor cleared, so the next attempt resends the full context automatically.
async fn attempt_once(
    conversation: &mut Conversation,
    request: &just_llm_client::types::generation::GenerationRequest,
) -> Attempt {
    match conversation.stream_generate(request.clone()).await {
        Ok(stream) => Attempt::Stream(stream),
        Err(error) => classify_error(error),
    }
}

/// Classify a backend error into a retry outcome (see [`attempt_once`]).
fn classify_error(error: BackendError) -> Attempt {
    let status = provider_rejection(&error).map(|rejection| rejection.status);
    match error {
        BackendError::Provider { .. } => match status {
            Some(s) if s == reqwest::StatusCode::TOO_MANY_REQUESTS => Attempt::Retry {
                error,
                kind: RetryKind::RateLimit,
            },
            Some(s) if s == reqwest::StatusCode::REQUEST_TIMEOUT => Attempt::Retry {
                error,
                kind: RetryKind::Timeout,
            },
            Some(s) if s.is_server_error() => Attempt::Retry {
                error,
                kind: RetryKind::Server,
            },
            Some(
                reqwest::StatusCode::UNAUTHORIZED
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND,
            ) => Attempt::Failover(error),
            Some(_) => Attempt::Fatal(error),
            None => Attempt::Retry {
                error,
                kind: RetryKind::Transport,
            },
        },
        // Pre-flight failures (invalid request, serialization, wire mismatch) fail
        // identically on every profile, so the failover loop must not advance.
        _ => Attempt::Fatal(error),
    }
}

/// Compute backoff delay for the given attempt with simple jitter.
pub(crate) fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
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

/// Call-scoped data for [`stream_with_retry`]: everything describing the request being sent and
/// its retry accounting. Side-channels (event sink, retry log, cancel token) stay as separate
/// parameters — different borrow modes, conceptually orthogonal to the call data.
pub struct RetryCall<'a> {
    pub conversation: &'a mut Conversation,
    pub request: just_llm_client::types::generation::GenerationRequest,
    pub policy: &'a RetryPolicy,
    pub round: usize,
    pub prior_retries: u32,
    pub endpoint_id: &'a str,
}

/// Open a streaming generation through the caller's conversation, retrying transient errors.
///
/// Sends the request up to `max_retries + 1` times through the caller's conversation. A transient
/// failure (HTTP 429/408/5xx, or a transport error) is retried with exponential backoff. An
/// endpoint-level permanent failure (401/403/404) is returned
/// as [`RequestFailure::Failover`] (a different profile may recover); a request-level permanent
/// failure (400/422) is returned as [`RequestFailure::Fatal`]; a cancel during backoff is returned
/// as [`RequestFailure::Cancelled`] (distinct from `Failover` so the caller short-circuits to a
/// cancelled round without advancing the failover chain).
///
/// Uses a per-endpoint retry budget: `prior_retries` is the number of recent retries (within
/// `retry_timeout`) already consumed against this endpoint, scoped by the caller. Remaining
/// budget = `max_retries - prior_retries`. The budget is endpoint-keyed (not profile-keyed)
/// because rate limits are endpoint-scoped; see the runner's failover loop for how
/// `prior_retries` is derived from the retry log.
///
/// On each retry attempt, emits an [`AgentEvent::Retrying`] via `event_tx` (best-effort,
/// non-blocking) and — once the backoff completes — appends a [`RetryRecord`] to `retry_log` (a
/// cancel-truncated backoff appends nothing). Returns the stream on success.
///
/// The caller is responsible for merging `retry_log` into persistent storage
/// (e.g., `ContextStore::retry_log`) and calling `persist()`.
pub async fn stream_with_retry(
    call: RetryCall<'_>,
    event_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    retry_log: &mut Vec<RetryRecord>,
    cancel: CancellationToken,
) -> Result<ConversationStream, RequestFailure> {
    let RetryCall {
        conversation,
        request,
        policy,
        round,
        prior_retries,
        endpoint_id,
    } = call;

    // Total sends this call = remaining retry budget + the initial attempt. `prior_retries`
    // exceeding `max_retries` saturates to zero, leaving exactly one (final) attempt.
    let max_attempts = policy.max_retries.saturating_sub(prior_retries) + 1;
    let deadline = tokio::time::Instant::now() + policy.retry_timeout;

    for attempt in 1..=max_attempts {
        match attempt_once(conversation, &request).await {
            Attempt::Stream(stream) => return Ok(stream),
            Attempt::Failover(error) => return Err(RequestFailure::Failover(error)),
            Attempt::Fatal(error) => return Err(RequestFailure::Fatal(error)),
            Attempt::Retry { error, kind } => {
                let body = crate::llm_error::http_body_for_log(&error);
                let error_msg = crate::llm_error::render_error(&error);
                // Budget exhausted — surface the final error without recording a retry.
                if attempt == max_attempts {
                    error!(
                        attempt = prior_retries + attempt,
                        body = ?body,
                        error = %error_msg,
                        "retry budget exhausted, advancing failover chain"
                    );
                    return Err(RequestFailure::Failover(error));
                }

                // Deadline first: if exhausted, don't record a retry we won't perform. Otherwise the
                // recorded `delay_secs` is the *actual* capped wait (backoff capped at the remaining
                // deadline), so telemetry never over-reports.
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    error!(
                        attempt = prior_retries + attempt,
                        body = ?body,
                        error = %error_msg,
                        "retry deadline exhausted, advancing failover chain"
                    );
                    return Err(RequestFailure::Failover(error));
                }
                let actual = backoff_delay(policy, attempt - 1).min(remaining);
                let delay_secs = actual.as_secs_f64();
                let global_attempt = prior_retries + attempt;

                warn!(
                    attempt = global_attempt,
                    max_attempts = policy.max_retries,
                    delay_secs,
                    body = ?body,
                    error = %error_msg,
                    "LLM request failed, retrying"
                );

                // Retrying event (live UI) — NON-BLOCKING: a full/wedged channel must not stall the
                // retry path (the Retrying event is best-effort telemetry).
                event_tx
                    .try_send(AgentEvent::Retrying {
                        attempt: global_attempt,
                        max_attempts: policy.max_retries,
                        error: error_msg.clone(),
                        delay_secs,
                    })
                    .ok();

                // Capture the trigger timestamp + build the record before the sleep, but push it only
                // once the retry actually happens: a cancel-truncated backoff must not persist
                // a "scheduled but unexecuted" retry.
                let record = RetryRecord {
                    timestamp: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    round,
                    attempt: global_attempt,
                    max_attempts: policy.max_retries,
                    error: error_msg,
                    delay_secs,
                    endpoint: Some(endpoint_id.to_string()),
                    kind,
                    quota_reset: None,
                };

                tokio::select! {
                    _ = tokio::time::sleep(actual) => {
                        retry_log.push(record);
                    }
                    _ = cancel.cancelled() => {
                        return Err(RequestFailure::Cancelled);
                    }
                }
            }
        }
    }

    // Every iteration returns or continues; the final attempt always returns, so this is
    // unreachable. `unreachable!` (not a silent fallback) so a future logic change can't mask here.
    unreachable!("retry loop always returns from within the for body")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RecordingBackend, request, user_msg};
    use just_llm_client::TransportError;

    // --- pure unit tests ---

    #[test]
    fn default_policy_pins_max_retries_at_ten() {
        // Nail the documented default (docs/reference/env.md, .env.example) so the two
        // code sites (here and config::defaults::DEFAULT_MAX_RETRIES) cannot drift from it silently.
        // Timing fields stay un-pinned here: the full-field mirror test in config/tests.rs
        // guards those against two-sided drift.
        assert_eq!(RetryPolicy::default().max_retries, 10);
    }

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

    // --- loop behavior via wiremock (real OpenAiCompat backend against a mock server) ---

    use crate::profile::GenerationClient;
    use futures_util::StreamExt;
    use just_llm_client::types::generation::GenerationEvent;
    use just_llm_client::{GenerationClientOptions, LlmBackend, provider::OpenAiCompatBackend};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    /// A real OpenAI-compatible backend pointed at a mock server, wrapped in a `GenerationClient`.
    fn mock_client(server: &MockServer) -> GenerationClient {
        let backend = OpenAiCompatBackend::new(
            reqwest::Client::builder().use_rustls_tls(),
            "test-key",
            Some(&server.uri()),
        )
        .expect("openai-compat backend constructs without network");
        GenerationClient::new(backend, GenerationClientOptions::new("gpt-4.1-mini"))
    }

    /// Fast policy so the suite stays snappy; caller sets `max_retries` per scenario.
    fn fast_policy(max_retries: u32) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            retry_timeout: Duration::from_secs(10),
        }
    }

    /// Count `AgentEvent::Retrying` events queued on `rx`.
    fn retrying_count(rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>) -> usize {
        let mut n = 0;
        while let Ok(AgentEvent::Retrying { .. }) = rx.try_recv() {
            n += 1;
        }
        n
    }

    async fn mount_status(server: &MockServer, status: u16) {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(status))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn retries_on_rate_limit_then_errors() {
        let server = MockServer::start().await;
        mount_status(&server, 429).await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &fast_policy(2),
                round: 0,
                prior_retries: 0,
                endpoint_id: "test",
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(result.is_err());
        // Transient retries that exhaust the budget surface as a Failover candidate.
        assert!(matches!(result, Err(RequestFailure::Failover(_))));
        assert_eq!(retry_log.len(), 2, "two retries before budget exhaustion");
        assert_eq!(retrying_count(&mut rx), 2);
    }

    #[tokio::test]
    async fn cancel_during_backoff_returns_cancelled_without_recording() {
        // A pre-cancelled token wins the backoff `select!` on the first retry → the outcome is
        // `Cancelled` (not `Failover`), and the truncated backoff pushes no record.
        let server = MockServer::start().await;
        mount_status(&server, 429).await; // forces a retry → backoff

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();
        let cancel = CancellationToken::new();
        cancel.cancel(); // already cancelled before the backoff select runs

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &fast_policy(2),
                round: 0,
                prior_retries: 0,
                endpoint_id: "test",
            },
            &tx,
            &mut retry_log,
            cancel,
        )
        .await;

        assert!(
            matches!(result, Err(RequestFailure::Cancelled)),
            "cancel during backoff surfaces as Cancelled, got {result:?}"
        );
        assert!(
            retry_log.is_empty(),
            "a cancel-truncated backoff must not persist a retry record"
        );
        // The Retrying event still fires pre-select (try_send is non-blocking but still emits).
        assert_eq!(retrying_count(&mut rx), 1);
    }

    #[tokio::test]
    async fn retries_on_server_error_then_errors() {
        let server = MockServer::start().await;
        mount_status(&server, 500).await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &fast_policy(2),
                round: 0,
                prior_retries: 0,
                endpoint_id: "test",
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(retry_log.len(), 2);
        assert_eq!(retrying_count(&mut rx), 2);
    }

    #[tokio::test]
    async fn does_not_retry_on_client_error() {
        let server = MockServer::start().await;
        mount_status(&server, 400).await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &fast_policy(3),
                round: 0,
                prior_retries: 0,
                endpoint_id: "test",
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        // 400 is request-level permanent — Fatal, not retried, not a failover candidate.
        assert!(matches!(result, Err(RequestFailure::Fatal(_))));
        assert!(retry_log.is_empty(), "fatal 4xx is not retried");
        assert_eq!(retrying_count(&mut rx), 0);
    }

    #[tokio::test]
    async fn failovers_on_not_found() {
        let server = MockServer::start().await;
        mount_status(&server, 404).await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &fast_policy(3),
                round: 0,
                prior_retries: 0,
                endpoint_id: "test",
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        // 404 is endpoint/profile-level — Failover (not retried in-profile, but another profile may
        // serve it). Wired into the failover loop in a later stage.
        assert!(matches!(result, Err(RequestFailure::Failover(_))));
        assert!(retry_log.is_empty(), "404 is not retried in-profile");
    }

    #[tokio::test]
    async fn failovers_on_auth_error() {
        let server = MockServer::start().await;
        mount_status(&server, 401).await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &fast_policy(3),
                round: 0,
                prior_retries: 0,
                endpoint_id: "test",
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        // 401 (auth) is endpoint-level — a different profile with different credentials may succeed.
        assert!(matches!(result, Err(RequestFailure::Failover(_))));
        assert!(retry_log.is_empty());
    }

    #[tokio::test]
    async fn retries_on_network_error() {
        // Point the backend at a port nothing listens on — `send` fails at the transport layer.
        let backend = OpenAiCompatBackend::new(
            reqwest::Client::builder().use_rustls_tls(),
            "test-key",
            Some("http://127.0.0.1:0"),
        )
        .expect("openai-compat backend constructs without network");
        let client = GenerationClient::new(backend, GenerationClientOptions::new("gpt-4.1-mini"));
        let mut conversation = client.conversation();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &fast_policy(1),
                round: 0,
                prior_retries: 0,
                endpoint_id: "test",
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(retry_log.len(), 1, "a network failure is retried");
        assert_eq!(retrying_count(&mut rx), 1);
    }

    #[tokio::test]
    async fn succeeds_on_ok_stream() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: {\"id\":\"chatcmpl-s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4.1-mini\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n",
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let mut stream = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &fast_policy(2),
                round: 0,
                prior_retries: 0,
                endpoint_id: "test",
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await
        .expect("2xx yields a stream");

        assert!(retry_log.is_empty());
        assert_eq!(retrying_count(&mut rx), 0);
        let event = stream.next().await.unwrap().unwrap();
        assert_eq!(
            event,
            GenerationEvent::Text {
                delta: "hi".to_owned()
            }
        );
    }
    #[tokio::test]
    async fn retries_on_request_timeout_classified_as_timeout() {
        // 408 keeps its own RetryKind (Timeout) through the BackendError classification path.
        let server = MockServer::start().await;
        mount_status(&server, 408).await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &fast_policy(1),
                round: 0,
                prior_retries: 0,
                endpoint_id: "test",
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(retry_log.len(), 1);
        assert!(matches!(retry_log[0].kind, RetryKind::Timeout));
        assert_eq!(retrying_count(&mut rx), 1);
    }

    // --- stateful chain behavior (recording backend, no network) ---

    use just_llm_client::types::generation::GenerationRequest;

    /// Drive one turn through `stream_with_retry` and drain the returned stream to
    /// completion — draining is what registers the turn's capture (`End` carries the
    /// response id), mirroring what `consume_stream` does in production.
    async fn run_turn(
        conversation: &mut Conversation,
        request: GenerationRequest,
    ) -> Result<(), RequestFailure> {
        let mut stream = stream_with_retry(
            RetryCall {
                conversation,
                request,
                policy: &fast_policy(2),
                round: 0,
                prior_retries: 0,
                endpoint_id: "test",
            },
            &tokio::sync::mpsc::channel(16).0,
            &mut Vec::new(),
            CancellationToken::new(),
        )
        .await?;
        while let Some(event) = stream.next().await {
            event.expect("turn events are healthy");
        }
        Ok(())
    }

    #[tokio::test]
    async fn stateful_chain_appends_with_previous_response_id() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        backend.queue_stream("resp-2");
        let client =
            GenerationClient::new(backend.clone(), GenerationClientOptions::new("test-model"));
        let mut conversation = client.conversation();

        // Turn 1: the full context goes out, storage on, no continuation id.
        run_turn(
            &mut conversation,
            request("test-model", vec![user_msg("q1")]).with_tools(vec![]),
        )
        .await
        .expect("turn 1 succeeds");

        // Turn 2: mirror + new user turn, as the runner composes it — the wire request is
        // the delta only, chained by response id, with tools stripped.
        let mirror = conversation.last_message().expect("turn 1 mirrored");
        run_turn(
            &mut conversation,
            request("test-model", vec![user_msg("q1"), mirror, user_msg("q2")]).with_tools(vec![]),
        )
        .await
        .expect("turn 2 succeeds");

        let requests = backend.take_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].previous_response_id, None);
        assert_eq!(requests[0].store, Some(true));
        assert_eq!(requests[0].messages, vec![user_msg("q1")]);
        assert!(
            requests[0].tools.is_some(),
            "the opening turn carries tools"
        );
        assert_eq!(requests[1].previous_response_id.as_deref(), Some("resp-1"));
        assert_eq!(requests[1].store, Some(true));
        assert_eq!(requests[1].messages, vec![user_msg("q2")], "delta only");
        assert!(requests[1].tools.is_none(), "continuation strips tools");
    }

    #[tokio::test]
    async fn settings_change_resends_full_context() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        backend.queue_stream("resp-2");
        let client =
            GenerationClient::new(backend.clone(), GenerationClientOptions::new("test-model"));
        let mut conversation = client.conversation();

        run_turn(
            &mut conversation,
            request("test-model", vec![user_msg("q1")]),
        )
        .await
        .expect("turn 1 succeeds");
        let mirror = conversation.last_message().expect("turn 1 mirrored");
        run_turn(
            &mut conversation,
            request("test-model", vec![user_msg("q1"), mirror, user_msg("q2")])
                .with_temperature(0.7),
        )
        .await
        .expect("turn 2 succeeds");

        let requests = backend.take_requests();
        assert_eq!(requests.len(), 2);
        // A settings change is not a pure append: the whole context goes out statelessly.
        assert_eq!(requests[1].previous_response_id, None);
        assert_eq!(requests[1].messages.len(), 3);
    }

    #[tokio::test]
    async fn provider_error_retries_and_resends_full_after_anchor_clear() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        backend.queue_stream("resp-2");
        let client =
            GenerationClient::new(backend.clone(), GenerationClientOptions::new("test-model"));
        let mut conversation = client.conversation();

        run_turn(
            &mut conversation,
            request("test-model", vec![user_msg("q1")]),
        )
        .await
        .expect("turn 1 succeeds");
        let mirror = conversation.last_message().expect("turn 1 mirrored");
        // Turn 2's first attempt is rate-limited; the anchor must clear so the retry
        // resends the full context instead of continuing an id the provider rejected.
        // (Queued after turn 1: the mock's error queue fires before any stream.)
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                body: "rate limited".to_owned(),
            },
        ));

        let mut retry_log = Vec::new();
        let mut stream = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("test-model", vec![user_msg("q1"), mirror, user_msg("q2")]),
                policy: &fast_policy(2),
                round: 0,
                prior_retries: 0,
                endpoint_id: "test",
            },
            &tokio::sync::mpsc::channel(16).0,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await
        .expect("turn 2 succeeds on retry");
        while let Some(event) = stream.next().await {
            event.expect("turn 2 events are healthy");
        }

        assert_eq!(retry_log.len(), 1);
        assert!(matches!(retry_log[0].kind, RetryKind::RateLimit));

        let requests = backend.take_requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].previous_response_id, None, "turn 1 opens full");
        assert_eq!(
            requests[1].previous_response_id.as_deref(),
            Some("resp-1"),
            "the rejected attempt chained normally"
        );
        assert_eq!(
            requests[2].previous_response_id, None,
            "after the provider rejection the anchor is cleared: full resend"
        );
        assert_eq!(requests[2].messages.len(), 3, "full context, not delta");
    }
}
