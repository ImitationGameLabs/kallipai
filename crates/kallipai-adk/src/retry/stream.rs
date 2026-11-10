//! The loop: one call in, one opened stream or one failure out.
//!
//! [`stream_with_retry`] takes a [`RetryCall`], asks the decision table what each
//! failure means, waits when it is told to, resends while the episode's budget
//! allows, and hands back a [`Sent`] when a stream opens or a [`RequestFailure`]
//! when it gives up. The rules it applies — the class table, the quota and
//! throttle handling, what each failure writes to the upstream fact table —
//! are described on the parent module.

use just_llm_client::{BackendError, Conversation, ConversationStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::event::AgentEvent;
use kallipai_common::facts::{BlockKind, ProfilePacing};
use kallipai_common::retry::{RetryAttempt, RetryRecord};
use kallipai_common::timefmt;

use super::budget::{RetryPolicy, record_fact, spend_shape_slot, spent_budget_error};
use super::decision;
use super::decision::Episode;
use super::wait::{jitter_now, sleep_unless_cancelled};

/// Call-scoped data for [`stream_with_retry`]: everything describing the request being sent and
/// its retry accounting. Side-channels (event sink, retry log, cancel token) stay as separate
/// parameters — different borrow modes, conceptually orthogonal to the call data.
pub struct RetryCall<'a> {
    pub conversation: &'a mut Conversation,
    pub request: just_llm_client::types::generation::GenerationRequest,
    pub policy: &'a RetryPolicy,
    pub round: usize,
    pub episode: &'a mut Episode,
    /// The provider this call connects through: the retry record's `provider` column
    /// carries it. Not the fact key — that is `profile_id`.
    pub provider_id: &'a str,
    /// The profile this call sends for: the key a fact it leaves behind is
    /// recorded under.
    pub profile_id: &'a str,
    /// The shared fact table, when the caller has one: a failure this call
    /// settles writes what it learned about that profile.
    pub pacing: Option<&'a dyn ProfilePacing>,
    /// Where this call's sends report. One observation per send: the send's own
    /// time, never the backoff before it.
    pub metrics: &'a crate::metrics::AdkMetrics,
}

/// A stream that opened, with the moment its send began.
///
/// The caller times the attempt from `started` at the point it learns how the
/// attempt ended, so a backoff between sends stays out of model time.
pub struct Sent {
    pub stream: ConversationStream,
    pub started: std::time::Instant,
}

impl std::fmt::Debug for Sent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sent { stream, started }")
    }
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
) -> Result<Sent, RequestFailure> {
    let RetryCall {
        conversation,
        request,
        policy,
        round,
        episode,
        provider_id,
        profile_id,
        pacing,
        metrics,
    } = call;

    // The episode's ledger counts the whole episode — retries and mid-stream
    // re-entries of earlier calls included. This answers the narrower question
    // a chat span asks: how many sends this one request needed.
    let mut sends: u32 = 0;

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
        sends += 1;
        // The span handed in is the chat span this request runs under
        // (`acquire_stream` instruments this future with it); the count belongs
        // to that request, not to the episode.
        let chat = tracing::Span::current();
        crate::spans::count_chat_send(&chat, sends);
        let send_started = std::time::Instant::now();
        let error = match conversation.stream_generate(request.clone()).await {
            Ok(stream) => {
                // A stream that opened is a window served: the record a paced
                // endpoint carries has done its job.
                if let Some(pacing) = pacing {
                    pacing.note_success(profile_id);
                }
                return Ok(Sent {
                    stream,
                    started: send_started,
                });
            }
            Err(error) => error,
        };
        // The send failed, so this attempt's outcome is known here: count it and
        // stop its clock now. A backoff before the next send is the policy's
        // time, not the model's, so it is never part of an attempt's duration.
        let outcome = match decision::classify(&error, decision::Phase::Connect) {
            decision::FailureClass::Transient(decision::TransientKind::Timeout) => {
                crate::metrics::ModelOutcome::Timeout
            }
            _ => crate::metrics::ModelOutcome::Error,
        };
        metrics.record_model_call(outcome, send_started.elapsed());
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
                    Some(provider_id.to_string()),
                    kind,
                    error_msg,
                    delay_secs,
                );

                if sleep_unless_cancelled(after, &cancel).await {
                    // The wait was allowed to finish: the retry happens, so it is recorded.
                    retry_log.push(record);
                } else {
                    // A cancel cut the wait short: nothing is recorded, and the
                    // round short-circuits without advancing the chain.
                    return Err(RequestFailure::Cancelled);
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
                // the fact table uses: the kinds that park the round are exactly
                // the stops that would otherwise leave no trace.
                let now = timefmt::now_epoch();
                // One evaluation of what this failure leaves behind: the record
                // predicate and the carrier the caller picks both read it.
                let fact = decision::fact_of(class, &error, now);
                let parks_the_round = fact
                    .as_ref()
                    .is_some_and(|block| block.kind.parks_the_round());
                if parks_the_round && let Some(kind) = decision::retry_kind(&class) {
                    retry_log.push(RetryRecord::observed(
                        RetryAttempt {
                            round,
                            attempt,
                            max_attempts: policy.max_retries,
                        },
                        now,
                        Some(provider_id.to_string()),
                        kind,
                        crate::llm_error::render_error(&error),
                        // The upstream's own moment, as it gave it: this record is
                        // the offline aggregation face (`reset − first rejection`),
                        // so the display trust rule does not apply here.
                        fact.as_ref().and_then(|block| block.until),
                    ));
                }
                record_fact(pacing, profile_id, class, &error, now);
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
        /// caller picks the carrier from it — a kind that parks the round carries
        /// the structured reason, any other kind stops with its wording.
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

#[cfg(test)]
mod tests;
