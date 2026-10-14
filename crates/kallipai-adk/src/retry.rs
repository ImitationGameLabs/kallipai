//! LLM request retry with exponential backoff.
//!
//! Retries transient failures (network errors, HTTP 408/5xx) at the
//! `just_llm_client::Conversation` boundary: each attempt asks the conversation to open a
//! streaming generation and classifies the resulting [`BackendError`] through the
//! `decision` table — by HTTP status for provider rejections, by variant otherwise. Only the
//! transient classes retry: a quota or payment rejection (429/402) advances the failover
//! chain instead, and the endpoint-level permanent classes (401/403/404) do the same. Both
//! write what they learned to the shared upstream fact table. The
//! retry budget is the episode's own attempt count ([`Episode`]), not the persisted retry
//! log: `max_retries` bounds the sends of one episode, `retry_timeout` bounds how long that
//! episode stays on trial. The server's `retry-after` header is not reachable through the
//! conversation, so backoff is the computed schedule only. A mid-stream failure is handled
//! by the acquisition loop, which continues the same episode.

use std::sync::Once;
use std::time::Duration;

use just_llm_client::{BackendError, Conversation, ConversationStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::event::AgentEvent;
use kallipai_common::facts::{BlockKind, EndpointHealth};
use kallipai_common::retry::{RetryAttempt, RetryRecord};
use kallipai_common::timefmt;

pub(crate) mod decision;

pub use decision::{AttemptOrigin, Episode};

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

/// Why a streaming request ultimately failed, surfaced to the failover loop. Transient retries
/// that exhaust the in-profile budget become [`RequestFailure::Failover`] (a different profile
/// may recover); request-level errors are [`RequestFailure::Fatal`]; a cancel during backoff is
/// [`RequestFailure::Cancelled`] (distinct from `Failover` so the caller short-circuits to a
/// cancelled round without advancing the failover chain).
///
/// A class another send cannot help with leaves through [`RequestFailure::Switch`]
/// (advance, then stop when the chain runs out) or [`RequestFailure::Stop`] (stop
/// without walking the chain). Both carry the class's own wording, which the
/// caller's stop message and log lines report.
#[derive(Debug, thiserror::Error)]
pub enum RequestFailure {
    /// The profile/endpoint cannot serve this request, but another profile might — advance.
    #[error("endpoint-level failure (failover candidate): {0}")]
    Failover(#[source] BackendError),
    /// A class the chain cannot help with by resending: advance to another profile
    /// while one is left, and stop when the chain runs out — never re-arm the
    /// transient re-run.
    #[error("endpoint-level failure (one-way switch): {error}")]
    Switch {
        /// The failure class, named for the caller's log line.
        reason: &'static str,
        #[source]
        error: BackendError,
        /// The kind of fact this failure leaves behind, when it leaves one: the
        /// caller picks the carrier from it — an account-level block parks with
        /// the structured reason, an endpoint-level one stops with its wording.
        block: Option<BlockKind>,
    },
    /// Stop now: the chain cannot help at all (no candidate left, or a local
    /// misconfiguration that fails on every profile).
    #[error("round stopped: {error}")]
    Stop {
        /// The failure class, named for the caller's stop message.
        reason: &'static str,
        #[source]
        error: BackendError,
    },
    /// The request itself is bad — fails on every profile; do not advance.
    #[error("request-level failure: {0}")]
    Fatal(#[source] BackendError),
    /// The round was cancelled during a retry backoff — short-circuit to a cancelled outcome; do
    /// not advance the failover chain.
    #[error("retry cancelled during backoff")]
    Cancelled,
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

/// The wait between two executions — the outer re-run, which sits outside any
/// episode: the backoff schedule with this attempt's jitter.
pub(crate) fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    decision::backoff_schedule(policy, attempt, jitter_now())
}

/// The jitter a wait carries, drawn from the operating system's random source and
/// capped at [`decision::RETRY_JITTER_MAX`]: mixing it in decorrelates retries
/// across agent instances started together.
///
/// The draw takes the source's 32-bit value modulo the cap. The cap does not
/// divide 2^32 evenly, so the lowest 29.5% of the range comes up once in 2^32
/// draws more often than the rest: 25% likelier there, and under 7% of the draws
/// overall. That is immaterial next to what the jitter is for, and removing it
/// would cost a rejection loop.
///
/// A source that fails falls back to zero jitter, which leaves the plain
/// schedule.
///
/// The retry core never reads the clock or a random source: it receives the
/// jitter as a value.
pub(crate) fn jitter_now() -> Duration {
    // A failed draw is reported once: this sits on the retry path, so a line per
    // attempt would repeat one fact.
    static REPORTED: Once = Once::new();
    let cap_nanos = decision::RETRY_JITTER_MAX.as_nanos() as u64;
    match getrandom::u32() {
        Ok(draw) => Duration::from_nanos(u64::from(draw) % cap_nanos),
        Err(error) => {
            REPORTED.call_once(|| {
                warn!(%error, "random source unavailable; jitter falls back to zero");
            });
            Duration::ZERO
        }
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
/// handle, or a class the table does not track, writes nothing.
pub(crate) fn record_endpoint_fact(
    health: Option<&dyn EndpointHealth>,
    endpoint_id: &str,
    class: decision::FailureClass,
    error: &BackendError,
    now: u64,
) {
    let Some(health) = health else { return };
    if let Some(block) = decision::fact_of(class, error, now) {
        health.record(endpoint_id, block);
    }
}

/// Call-scoped data for [`stream_with_retry`]: everything describing the request being sent and
/// its retry accounting. Side-channels (event sink, retry log, cancel token) stay as separate
/// parameters — different borrow modes, conceptually orthogonal to the call data.
pub struct RetryCall<'a> {
    pub conversation: &'a mut Conversation,
    pub request: just_llm_client::types::generation::GenerationRequest,
    pub policy: &'a RetryPolicy,
    pub round: usize,
    pub episode: &'a mut Episode,
    pub endpoint_id: &'a str,
    /// The shared fact table, when the caller has one: a failure this call
    /// settles writes what it learned about the endpoint or the account.
    pub health: Option<&'a dyn EndpointHealth>,
}

/// Open a streaming generation through the caller's conversation, retrying transient errors.
///
/// Sends the request while the episode has budget left: at most `max_retries + 1`
/// sends per episode, minus the sends earlier calls in the same episode already
/// charged (a mid-stream re-entry continues the same ledger). The budget is the
/// episode's own attempt count; the persisted retry log is an observation surface
/// and never shrinks it. A transient failure (HTTP 408/5xx, a transport error, a
/// truncated body) is retried with exponential backoff. An endpoint-level
/// permanent failure (401/403/404) returns [`RequestFailure::Failover`] — a
/// different profile may recover. A request-level permanent failure (400/422)
/// returns [`RequestFailure::Fatal`]. A cancel during backoff returns
/// [`RequestFailure::Cancelled`], distinct from `Failover` so the caller
/// short-circuits to a cancelled round without advancing the failover chain.
///
/// The episode's time on trial is bounded by `retry_timeout`, measured from its
/// first attempt and compared only when a failure is being decided: the deadline
/// arms no timer and never interrupts a send in flight. The wait before a retry is
/// the backoff capped by what is left of that deadline, so a recorded `delay_secs`
/// is the wait that actually happened.
///
/// What to do about each outcome comes from the core: `decision::decide` reads
/// the episode's ledger and the failure into one verdict, and this function only
/// performs it. The wait it sleeps is the verdict's own, already clamped.
///
/// On each retry attempt, emits an [`AgentEvent::Retrying`] via `event_tx`
/// (best-effort, non-blocking) and — once the backoff completes — appends a
/// [`RetryRecord`] to `retry_log` (a cancel-truncated backoff appends nothing).
/// Returns the stream on success.
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
        episode,
        endpoint_id,
        health,
    } = call;

    loop {
        // Before each send: the core's verdict on the episode's ledger. This is
        // the fail-closed guard, not the preflight: the table reading is taken in
        // the acquisition loop, which owns the chain and the table handle, and
        // this call re-sends with no reading of its own, so a fact another agent
        // wrote during a backoff is not re-checked here — the loop entry above
        // reads the table again before the next attempt. What is left here is the
        // ledger, so a verdict other than `Proceed` means the episode is spent
        // — a caller bug, since the loop pre-checks the same reading — and
        // erroring the round beats panicking the agent task.
        let pre_send = decision::Attempt::pre_send(
            decision::Phase::Connect,
            decision::AttemptOrigin::System,
            timefmt::now_epoch(),
            decision::TableReading::none(),
        );
        match decision::decide(&episode.facts(&pre_send), policy) {
            decision::Decision::Proceed => {}
            other => {
                error!(
                    attempt = episode.attempts(),
                    max_attempts = policy.max_retries,
                    verdict = ?other,
                    "retry budget already spent for this episode, failing the round"
                );
                return Err(RequestFailure::Fatal(spent_budget_error()));
            }
        }

        // Charge the send before making it: an attempt cancelled in flight still
        // happened, and this count is the budget.
        *episode = episode.after_attempt();
        let error = match conversation.stream_generate(request.clone()).await {
            Ok(stream) => return Ok(stream),
            Err(error) => error,
        };
        let attempt = episode.attempts();
        let class = decision::classify(&error, decision::Phase::Connect);
        let attempt_facts = decision::Attempt::failed(
            decision::Phase::Connect,
            class,
            decision::shape_key(&error),
            timefmt::now_epoch(),
            jitter_now(),
        );

        match decision::decide(&episode.facts(&attempt_facts), policy) {
            // A failure always yields a verdict about it: `Proceed` is the
            // pre-send reading, and that reading is taken above.
            decision::Decision::Proceed => return Err(RequestFailure::Fatal(error)),
            decision::Decision::Retry { after } => {
                let Some(kind) = decision::retry_kind(&class) else {
                    // A retryable class always maps to a record kind; failing the
                    // round beats recording a retry under a made-up one.
                    return Err(RequestFailure::Fatal(error));
                };
                let body = crate::llm_error::http_body_for_log(&error);
                let error_msg = crate::llm_error::render_error(&error);

                // A shape-class retry spends this episode's one shape slot for
                // that key, so a repeat of the same shape exits as endpoint-level.
                // Only a retry this path performs spends the slot.
                spend_shape_slot(episode, &error);
                let delay_secs = after.as_secs_f64();

                warn!(
                    attempt,
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
                        attempt,
                        max_attempts: policy.max_retries,
                        error: error_msg.clone(),
                        delay_secs,
                    })
                    .ok();

                // Capture the trigger timestamp + build the record before the sleep, but push it only
                // once the retry actually happens: a cancel-truncated backoff must not persist
                // a "scheduled but unexecuted" retry.
                let record = RetryRecord::retry(
                    RetryAttempt {
                        round,
                        attempt,
                        max_attempts: policy.max_retries,
                    },
                    timefmt::now_epoch(),
                    Some(endpoint_id.to_string()),
                    kind,
                    error_msg,
                    delay_secs,
                );

                tokio::select! {
                    _ = tokio::time::sleep(after) => {
                        retry_log.push(record);
                    }
                    _ = cancel.cancelled() => {
                        return Err(RequestFailure::Cancelled);
                    }
                }
            }
            decision::Decision::AdvanceFailover { reason } => {
                // A quota rejection stops here without ever reaching the retry
                // arm, so its failure would leave no trace at all: write the
                // observation with no wait, its kind from the one kind table. A
                // shape repeat stops after the first shape failure recorded its
                // retry, so it adds no second record.
                //
                // Which classes get that record comes from the same core mapping
                // the fact table uses: the account-level facts are exactly the
                // stops that would otherwise leave no trace.
                let now = timefmt::now_epoch();
                // One evaluation of what this failure leaves behind: the record
                // predicate and the carrier the caller picks both read it.
                let fact = decision::fact_of(class, &error, now);
                let account_level = fact
                    .as_ref()
                    .is_some_and(|block| block.kind.is_account_level());
                if account_level && let Some(kind) = decision::retry_kind(&class) {
                    retry_log.push(RetryRecord::observed(
                        RetryAttempt {
                            round,
                            attempt,
                            max_attempts: policy.max_retries,
                        },
                        now,
                        Some(endpoint_id.to_string()),
                        kind,
                        crate::llm_error::render_error(&error),
                        // The upstream's own moment, as it gave it: this record is
                        // the offline aggregation face (`reset − first rejection`),
                        // so the display trust rule does not apply here.
                        fact.as_ref().and_then(|block| block.until),
                    ));
                }
                record_endpoint_fact(health, endpoint_id, class, &error, now);
                return Err(RequestFailure::Switch {
                    reason,
                    error,
                    block: fact.map(|block| block.kind),
                });
            }
            decision::Decision::Stop {
                kind: decision::StopKind::ExternalOnly,
                reason,
            } => return Err(RequestFailure::Stop { reason, error }),
            // The episode's budget or deadline ran out: the chain takes over, and
            // the outer re-run budget decides whether the round is retried later.
            decision::Decision::Stop {
                kind: decision::StopKind::ChainExhausted,
                reason,
            } => {
                error!(
                    attempt,
                    body = ?crate::llm_error::http_body_for_log(&error),
                    error = %crate::llm_error::render_error(&error),
                    reason,
                    "retry budget or deadline spent, advancing failover chain"
                );
                return Err(RequestFailure::Failover(error));
            }
            decision::Decision::Fatal { reason } => {
                debug!(
                    reason,
                    error = %crate::llm_error::render_error(&error),
                    "request-level failure, failing the round"
                );
                return Err(RequestFailure::Fatal(error));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RecordingBackend, RecordingHealth, request, user_msg};
    use just_llm_client::TransportError;
    use kallipai_common::facts::BlockKind;
    use kallipai_common::retry::RetryKind;
    use std::sync::Arc;

    // --- pure unit tests ---

    #[test]
    fn default_policy_pins_max_retries_at_ten() {
        // Nail the documented default (docs/en/reference/environment-variables/, .env.example) so
        // the two code sites (here and config::defaults::DEFAULT_MAX_RETRIES) cannot drift from it
        // silently.
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
        assert!(d <= Duration::from_secs(5) + decision::RETRY_JITTER_MAX);
    }

    #[test]
    fn the_drawn_jitter_stays_under_the_cap_and_varies() {
        // The cap is what the wait's upper bound rests on, so every draw has to sit
        // below it whatever the random source hands back. The draws also have to
        // differ from one another: a constant, or a fallback to zero, would leave
        // every retry waiting the same and the decorrelation unpaid for.
        let draws: Vec<Duration> = (0..64).map(|_| jitter_now()).collect();
        assert!(draws.iter().all(|d| *d < decision::RETRY_JITTER_MAX));
        assert!(
            draws.iter().any(|d| *d != draws[0]),
            "every draw came back the same: the source is not varying the wait"
        );
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
    async fn rate_limit_switches_without_retrying() {
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
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        // A quota rejection does not recover by resending the same request: the
        // round hands the switch to the caller, and the endpoint saw exactly one
        // send. The failure is still an observation: one record with no wait.
        assert!(matches!(
            result,
            Err(RequestFailure::Switch {
                reason: "upstream quota or payment rejection",
                ..
            })
        ));
        assert_eq!(retry_log.len(), 1, "a quota rejection writes one record");
        assert!(matches!(retry_log[0].kind, RetryKind::RateLimit));
        assert_eq!(retry_log[0].delay_secs, 0.0, "no wait precedes the switch");
        assert_eq!(
            retry_log[0].quota_reset, None,
            "a bare rejection carries no reset hint"
        );
        assert_eq!(retrying_count(&mut rx), 0);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "the same endpoint is not sent again"
        );
    }

    #[tokio::test]
    async fn cancel_during_backoff_returns_cancelled_without_recording() {
        // A pre-cancelled token wins the backoff `select!` on the first retry → the outcome is
        // `Cancelled` (not `Failover`), and the truncated backoff pushes no record.
        let server = MockServer::start().await;
        mount_status(&server, 500).await; // forces a retry → backoff

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
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
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
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
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
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
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
    async fn switches_on_not_found() {
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
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        // 404 is endpoint/profile-level: not retried in-profile, and the round hands
        // the switch to the caller, which advances the chain.
        assert!(matches!(
            result,
            Err(RequestFailure::Switch {
                reason: "endpoint or credentials rejected the request",
                ..
            })
        ));
        assert!(retry_log.is_empty(), "404 is not retried in-profile");
    }

    #[tokio::test]
    async fn switches_on_auth_error() {
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
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        // 401 (auth) is endpoint-level: the round hands the switch to the caller, and a
        // different profile with different credentials may succeed.
        assert!(matches!(
            result,
            Err(RequestFailure::Switch {
                reason: "endpoint or credentials rejected the request",
                ..
            })
        ));
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
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
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
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
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
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
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

    /// Drive one turn through `stream_with_retry`, drain the stream to completion, and
    /// adopt the assembled response — the explicit three-step contract the production
    /// runner follows (`consume_stream` drains, `acquire_stream` adopts).
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
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
            },
            &tokio::sync::mpsc::channel(16).0,
            &mut Vec::new(),
            CancellationToken::new(),
        )
        .await?;
        while let Some(event) = stream.next().await {
            event.expect("turn events are healthy");
        }
        let response = stream
            .into_response()
            .expect("a drained test stream reaches `End` with a response id");
        conversation
            .adopt(response)
            .expect("the just-prepared turn adopts cleanly");
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
        assert!(
            requests[1].tools.is_some(),
            "continuation keeps the caller's tools"
        );
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
    async fn provider_error_retries_and_resends_delta_with_anchor_kept() {
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
        // Turn 2's first attempt fails at the provider; a failed turn keeps the anchor
        // (the stored response stays valid server-side), so the retry continues it with
        // a delta. (Queued after turn 1: the mock's error queue fires before any stream.)
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                body: "upstream failed".to_owned(),
            },
        ));

        let mut retry_log = Vec::new();
        let mut stream = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("test-model", vec![user_msg("q1"), mirror, user_msg("q2")]),
                policy: &fast_policy(2),
                round: 0,
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
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
        assert!(matches!(retry_log[0].kind, RetryKind::Server));

        let requests = backend.take_requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].previous_response_id, None, "turn 1 opens full");
        assert_eq!(
            requests[1].previous_response_id.as_deref(),
            Some("resp-1"),
            "the rejected attempt chained normally"
        );
        assert_eq!(
            requests[2].previous_response_id.as_deref(),
            Some("resp-1"),
            "a failed turn keeps the anchor: the retry continues the stored turn"
        );
        assert_eq!(
            requests[2].messages,
            vec![user_msg("q2")],
            "delta only — the stored prefix is not resent"
        );
    }

    // --- the episode ledger is the budget ---

    /// An episode that already spent its sends makes no request: the entry guard
    /// refuses before the wire, and the round fails instead of retrying.
    #[tokio::test]
    async fn a_spent_episode_sends_nothing() {
        let server = MockServer::start().await;
        mount_status(&server, 429).await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();
        let policy = fast_policy(2);
        let mut episode = Episode::new(timefmt::now_epoch());
        for _ in 0..max_sends(&policy) {
            episode = episode.after_attempt();
        }

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &policy,
                round: 0,
                episode: &mut episode,
                endpoint_id: "test",
                health: None,
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(matches!(result, Err(RequestFailure::Fatal(_))));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            0,
            "a spent ledger sends nothing"
        );
        assert!(retry_log.is_empty());
        assert_eq!(retrying_count(&mut rx), 0);
    }

    /// Sends already charged elsewhere in the episode shrink this call's budget: the
    /// ledger, not the persisted retry log, decides how many sends remain.
    #[tokio::test]
    async fn a_partly_spent_episode_spends_only_what_is_left() {
        let server = MockServer::start().await;
        mount_status(&server, 500).await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();
        let policy = fast_policy(2);
        // Two of the episode's three sends are gone: one send is left, and its
        // failure exhausts the budget without a retry.
        let mut episode = Episode::new(timefmt::now_epoch());
        episode = episode.after_attempt();
        episode = episode.after_attempt();

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &policy,
                round: 0,
                episode: &mut episode,
                endpoint_id: "test",
                health: None,
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(matches!(result, Err(RequestFailure::Failover(_))));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "exactly the one send the ledger allowed"
        );
        assert_eq!(episode.attempts(), 3, "the send charged the ledger");
        assert!(retry_log.is_empty(), "an exhausted budget records no retry");
        assert_eq!(retrying_count(&mut rx), 0);
    }

    /// The deadline runs from the episode's start, not from this call: an episode
    /// whose `retry_timeout` is spent fails the round without scheduling a retry.
    #[tokio::test]
    async fn the_deadline_is_measured_from_the_episode_start() {
        let server = MockServer::start().await;
        mount_status(&server, 500).await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();
        let policy = RetryPolicy {
            retry_timeout: Duration::from_secs(1),
            ..fast_policy(3)
        };
        let mut episode = Episode::new(timefmt::now_epoch() - 5);

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &policy,
                round: 0,
                episode: &mut episode,
                endpoint_id: "test",
                health: None,
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(matches!(result, Err(RequestFailure::Failover(_))));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(
            retry_log.is_empty(),
            "no retry is recorded when the deadline is spent"
        );
        assert_eq!(retrying_count(&mut rx), 0);
    }

    /// The wait before a retry is capped by what is left of the deadline, so a
    /// recorded `delay_secs` is the wait that actually happened.
    #[tokio::test]
    async fn the_recorded_wait_is_capped_by_the_remaining_deadline() {
        let server = MockServer::start().await;
        mount_status(&server, 503).await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();
        // The backoff schedule asks for 30s; the episode has a few seconds left.
        let policy = RetryPolicy {
            max_retries: 1,
            base_delay: Duration::from_secs(30),
            max_delay: Duration::from_secs(30),
            retry_timeout: Duration::from_secs(10),
        };
        let mut episode = Episode::new(timefmt::now_epoch() - 7);

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &policy,
                round: 0,
                episode: &mut episode,
                endpoint_id: "test",
                health: None,
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(matches!(result, Err(RequestFailure::Failover(_))));
        assert_eq!(retry_log.len(), 1);
        assert_eq!(retrying_count(&mut rx), 1);
        let waited = retry_log[0].delay_secs;
        assert!(
            (0.5..=3.0).contains(&waited),
            "the wait is the remaining deadline, not the schedule: {waited}"
        );
    }

    /// A response shape that fails the same way twice is not a transient problem:
    /// the first shape failure retries, the repeat leaves through the switch.
    #[tokio::test]
    async fn a_repeated_response_shape_stops_the_episode() {
        let backend = RecordingBackend::new();
        for _ in 0..2 {
            backend.queue_error(BackendError::provider(
                "recording",
                TransportError::InvalidResponse("empty body".into()),
            ));
        }
        let client =
            GenerationClient::new(backend.clone(), GenerationClientOptions::new("test-model"));
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("test-model", vec![user_msg("q1")]),
                policy: &fast_policy(2),
                round: 0,
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            result,
            Err(RequestFailure::Switch {
                reason: "the response shape failed twice in this episode",
                ..
            })
        ));
        assert_eq!(
            backend.take_requests().len(),
            2,
            "the first shape failure retries once; the repeat stops"
        );
        assert_eq!(retry_log.len(), 1, "the repeat records no retry");
        assert_eq!(retrying_count(&mut rx), 1);
    }

    /// A client-construction failure is local: no send can fix it, so the round stops
    /// on the first answer with no retry recorded.
    #[tokio::test]
    async fn a_local_configuration_error_stops_without_retrying() {
        let backend = RecordingBackend::new();
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::InvalidConfig("no key"),
        ));
        let client =
            GenerationClient::new(backend.clone(), GenerationClientOptions::new("test-model"));
        let mut conversation = client.conversation();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("test-model", vec![user_msg("q1")]),
                policy: &fast_policy(2),
                round: 0,
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            result,
            Err(RequestFailure::Stop {
                reason: "local client configuration error",
                ..
            })
        ));
        assert_eq!(backend.take_requests().len(), 1);
        assert!(retry_log.is_empty());
        assert_eq!(retrying_count(&mut rx), 0);
    }
    /// The connect path writes what the rejection taught: the 429 that stops this
    /// endpoint leaves an account-level quota fact, under the provider id the
    /// call was made through.
    #[tokio::test]
    async fn the_connect_path_writes_the_fact_a_rejection_leaves() {
        let server = MockServer::start().await;
        mount_status(&server, 429).await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();
        let health = Arc::new(RecordingHealth::default());

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &fast_policy(2),
                round: 0,
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: Some(health.as_ref()),
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(matches!(result, Err(RequestFailure::Switch { .. })));
        let recorded = health.recorded();
        assert_eq!(recorded.len(), 1, "one rejection, one fact");
        assert_eq!(
            recorded[0].0, "test",
            "the fact names the provider it was observed through"
        );
        assert_eq!(recorded[0].1.kind, BlockKind::Quota);
        assert!(
            recorded[0].1.kind.is_account_level(),
            "a quota rejection follows the credential, not the endpoint"
        );
    }

    /// A rejection that names when service resumes hands that moment to the
    /// record: it is the offline aggregation face, so the upstream's own value
    /// rides it as given.
    #[tokio::test]
    async fn a_quota_rejection_records_the_reset_hint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_string("{\"error\":\"slow down\",\"retry_after\":120}"),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server);
        let mut conversation = client.conversation();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut retry_log = Vec::new();
        let before = timefmt::now_epoch();

        let result = stream_with_retry(
            RetryCall {
                conversation: &mut conversation,
                request: request("gpt-4.1-mini", vec![user_msg("hi")]),
                policy: &fast_policy(2),
                round: 0,
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "test",
                health: None,
            },
            &tx,
            &mut retry_log,
            CancellationToken::new(),
        )
        .await;

        assert!(matches!(result, Err(RequestFailure::Switch { .. })));
        assert_eq!(retry_log.len(), 1, "one rejection, one record");
        assert!(matches!(retry_log[0].kind, RetryKind::RateLimit));
        assert_eq!(retry_log[0].delay_secs, 0.0);
        let reset = retry_log[0].quota_reset.expect("the hint rides the record");
        assert!(reset >= before + 120, "the upstream's own moment: {reset}");
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
