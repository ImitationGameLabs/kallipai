//! LLM request retry with exponential backoff.
//!
//! Retries transient failures (network errors, HTTP 429/5xx) at the
//! [`LlmBackend`](just_llm_client::LlmBackend) prepare/send/parse boundary. The retry decision
//! reads the raw HTTP `status` — and the server's `retry-after` header — directly off the response
//! *before* [`parse_streaming`](just_llm_client::LlmBackend::parse_streaming) converts a non-2xx
//! response into an error, so it never depends on error-source introspection. Once content deltas
//! start flowing, retry is off the table — mid-stream failures propagate as errors.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use just_llm_client::{BackendError, ChatCompletionStream};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::event::AgentEvent;
use kallip_common::retry::RetryRecord;

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

/// Outcome of a single prepare/send/parse attempt.
enum Attempt {
    /// 2xx — a live stream ready to hand off to the caller.
    Stream(ChatCompletionStream),
    /// Transient failure (HTTP 429/5xx/408, or a network send failure). Worth retrying in-profile.
    Retry {
        error: BackendError,
        retry_after: Option<Duration>,
    },
    /// Endpoint/profile-level permanent failure (401/403/404). A different profile (different
    /// credentials / endpoint / model) may succeed, so the failover loop advances the chain.
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

/// Whether an HTTP status warrants an in-profile retry: rate-limit (`429`), request timeout
/// (`408`), and server errors (`5xx`).
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status.is_server_error()
}

/// Whether an HTTP status is an endpoint/profile-level permanent failure worth failing over:
/// auth (`401`/`403`) and model-not-found (`404`). A different profile may succeed.
fn is_failover_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED
            | reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::NOT_FOUND
    )
}

/// Parse the `retry-after` header, delta-seconds form only.
///
/// The HTTP-date form is intentionally unsupported and yields `None` (callers fall back to the
/// computed backoff). Delta-seconds is the universal form for LLM rate-limiting.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let secs: u64 = value.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Run one attempt: send a clone of the prepared request, then classify the outcome.
///
/// `prepare_streaming` ran once in [`stream_with_retry`] (it is validation + serialization only,
/// so its failure is deterministic and surfaced before this loop). `send` returns the raw response
/// without checking status, so `retry-after` and `status` are read here, before `parse_streaming`
/// consumes the body. `parse_streaming` runs `ensure_success`, so a non-2xx status surfaces as a
/// [`BackendError`] — the retry decision is then a direct status check, not error-source
/// archaeology. A `send` failure is transport-level (connect/timeout/...) and is always transient.
async fn attempt_once(client: &crate::profile::ChatClient, prepared: &reqwest::Request) -> Attempt {
    // `reqwest::Request` has no `Clone` impl, but `try_clone` succeeds because the provider sets
    // the body from buffered JSON bytes (never a stream). Holds for every request this codebase
    // builds; a non-clonable body would indicate a backend bug.
    let to_send = prepared
        .try_clone()
        .expect("provider request bodies are buffered bytes and therefore clonable");

    let response = match client.send(to_send).await {
        Ok(response) => response,
        Err(error) => {
            return Attempt::Retry {
                error,
                retry_after: None,
            };
        }
    };

    let status = response.status();
    let retry_after = parse_retry_after(response.headers());
    match client.parse_streaming(response).await {
        Ok(stream) => Attempt::Stream(stream),
        Err(error) => {
            if is_retryable_status(status) {
                Attempt::Retry { error, retry_after }
            } else if is_failover_status(status) {
                Attempt::Failover(error)
            } else {
                Attempt::Fatal(error)
            }
        }
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

/// Call-scoped data for [`stream_with_retry`]: everything describing the request being sent and
/// its retry accounting. Side-channels (event sink, retry log, cancel token) stay as separate
/// parameters — different borrow modes, conceptually orthogonal to the call data.
pub struct RetryCall<'a> {
    pub client: &'a crate::profile::ChatClient,
    pub request: just_llm_client::types::chat::ChatCompletionRequest,
    pub policy: &'a RetryPolicy,
    pub round: usize,
    pub prior_retries: u32,
    pub endpoint_id: &'a str,
}

/// Open a streaming chat completion with retry on transient errors.
///
/// Prepares the request once, then sends it up to `max_retries + 1` times. A transient failure
/// (HTTP 429/5xx, or a network error) is retried with exponential backoff that floors on the
/// server's `retry-after` header. An endpoint-level permanent failure (401/403/404) is returned
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
) -> Result<ChatCompletionStream, RequestFailure> {
    let RetryCall {
        client,
        request,
        policy,
        round,
        prior_retries,
        endpoint_id,
    } = call;
    // Prepare once: validation + serialization only, so a failure here is deterministic — surface
    // it immediately rather than retrying a request that can never succeed.
    let prepared = client
        .prepare_streaming(request)
        .map_err(RequestFailure::Fatal)?;

    // Total sends this call = remaining retry budget + the initial attempt. `prior_retries`
    // exceeding `max_retries` saturates to zero, leaving exactly one (final) attempt.
    let max_attempts = policy.max_retries.saturating_sub(prior_retries) + 1;
    let deadline = tokio::time::Instant::now() + policy.retry_timeout;

    for attempt in 1..=max_attempts {
        match attempt_once(client, &prepared).await {
            Attempt::Stream(stream) => return Ok(stream),
            Attempt::Failover(error) => return Err(RequestFailure::Failover(error)),
            Attempt::Fatal(error) => return Err(RequestFailure::Fatal(error)),
            Attempt::Retry { error, retry_after } => {
                // Budget exhausted — surface the final error without recording a retry.
                if attempt == max_attempts {
                    return Err(RequestFailure::Failover(error));
                }

                // Deadline first: if exhausted, don't record a retry we won't perform. Otherwise the
                // recorded `delay_secs` is the *actual* capped wait (floor on retry-after, then cap
                // at the remaining deadline), so telemetry never over-reports.
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(RequestFailure::Failover(error));
                }
                let actual = backoff_delay(policy, attempt - 1)
                    .max(retry_after.unwrap_or_default())
                    .min(remaining);
                let delay_secs = actual.as_secs_f64();
                let error_msg = format!("{error:#}");
                let global_attempt = prior_retries + attempt;

                info!(
                    attempt = global_attempt,
                    max_attempts = policy.max_retries,
                    delay_secs,
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

    // --- pure unit tests ---

    #[test]
    fn is_retryable_status_matches_rate_limit_and_server_errors() {
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(reqwest::StatusCode::GATEWAY_TIMEOUT));
        // 408 (Request Timeout) is transient.
        assert!(is_retryable_status(reqwest::StatusCode::REQUEST_TIMEOUT));

        // Not retryable.
        assert!(!is_retryable_status(reqwest::StatusCode::OK));
        assert!(!is_retryable_status(reqwest::StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn is_failover_status_matches_auth_and_not_found() {
        // Endpoint/profile-level — a different profile (credentials / model) may recover.
        assert!(is_failover_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(is_failover_status(reqwest::StatusCode::FORBIDDEN));
        assert!(is_failover_status(reqwest::StatusCode::NOT_FOUND));

        // Not failover-class.
        assert!(!is_failover_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(!is_failover_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(!is_failover_status(reqwest::StatusCode::BAD_REQUEST)); // request-level → Fatal
        assert!(!is_failover_status(reqwest::StatusCode::OK));
    }

    #[test]
    fn parse_retry_after_reads_delta_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("30"),
        );
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(30)));

        // Garbage and HTTP-date form yield None (fall back to computed backoff).
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("Wed, 21 Oct 2025 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&headers), None);

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("soon"),
        );
        assert_eq!(parse_retry_after(&headers), None);

        // Absent.
        assert_eq!(parse_retry_after(&reqwest::header::HeaderMap::new()), None);
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

    use crate::profile::ChatClient;
    use futures_util::StreamExt;
    use just_llm_client::{
        ChatClientOptions, LlmBackend,
        provider::OpenAiCompatBackend,
        types::chat::{ChatCompletionRequest, ChatMessage},
    };
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    /// A real OpenAI-compatible backend pointed at a mock server, wrapped in a `ChatClient`.
    fn mock_client(server: &MockServer) -> ChatClient {
        let backend = OpenAiCompatBackend::new(
            reqwest::Client::builder().use_rustls_tls(),
            "test-key",
            Some(&server.uri()),
        )
        .expect("openai-compat backend constructs without network");
        ChatClient::new(backend, ChatClientOptions::new("gpt-4.1-mini"))
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

    fn trivial_request() -> ChatCompletionRequest {
        ChatCompletionRequest::new("gpt-4.1-mini", vec![ChatMessage::user("hi")])
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
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                client: &client,
                request: trivial_request(),
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
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();
        let cancel = CancellationToken::new();
        cancel.cancel(); // already cancelled before the backoff select runs

        let result = stream_with_retry(
            RetryCall {
                client: &client,
                request: trivial_request(),
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
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                client: &client,
                request: trivial_request(),
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
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                client: &client,
                request: trivial_request(),
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
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                client: &client,
                request: trivial_request(),
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
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                client: &client,
                request: trivial_request(),
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
    async fn honors_retry_after_header_as_backoff_floor() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .mount(&server)
            .await;

        let client = mock_client(&server);
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                client: &client,
                request: trivial_request(),
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
        // retry-after (1s) dominates the ~1ms computed backoff.
        assert!(
            retry_log[0].delay_secs >= 1.0,
            "retry-after must floor the delay: {}",
            retry_log[0].delay_secs
        );
    }

    #[tokio::test]
    async fn falls_back_to_backoff_when_retry_after_is_http_date() {
        // The HTTP-date form isn't parsed (delta-seconds only), so it must yield None — and the
        // 429 is still retried using the computed backoff (not treated as permanent).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "Wed, 21 Oct 2025 07:28:00 GMT"),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server);
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                client: &client,
                request: trivial_request(),
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
        assert_eq!(retry_log.len(), 1, "HTTP-date retry-after still retries");
        // Computed backoff (~1ms) is used, not a misparsed HTTP-date value.
        assert!(retry_log[0].delay_secs < 1.0);
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
        let client = ChatClient::new(backend, ChatClientOptions::new("gpt-4.1-mini"));

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                client: &client,
                request: trivial_request(),
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
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let mut stream = stream_with_retry(
            RetryCall {
                client: &client,
                request: trivial_request(),
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

        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
    }
}
