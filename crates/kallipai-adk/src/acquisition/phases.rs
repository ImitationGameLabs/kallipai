//! The within-set acquisition loop and the stages it drives.
//!
//! One attempt at a time: settle the pre-send verdict against the episode ledger and the
//! shared fact table (`enter_attempt`), rebuild the request from the failover state
//! (`build_request`), send it through the retry shell, and classify what came back. A
//! stream that drops mid-way is decided by the same table (`on_mid_stream_failure`):
//! reset and re-acquire in place, or walk the chain — whose advance lives in
//! `super::failover` — with `walk_or_finish` and `void_partial`. The image-rejection
//! degrade re-acquires once with the images stripped, which is why it sits next to the
//! loop it re-enters.

use super::facts::{
    emit_fact_signal, fact_chain_note, fact_error, fact_stop_message, log_fact_skip,
    quota_blocked_payload, table_reading,
};
use super::failover::{
    Exhaustion, FailedSend, FailoverStep, LOCAL_CONFIG_NOTE, step_failover, stop_message,
};
use super::stream::{AcquireResult, StreamOutcome, consume_stream, fatal_provider_status};
use super::{flush_retry_records, retry_keep_since};

use crate::agent_task::AgentContext;
use crate::context::{compose_context, message_has_images, strip_message_images};
use crate::event::{AgentEvent, AgentOutcome};
use crate::retry::decision::{
    self, Attempt, AttemptOrigin, Decision, Phase, StopKind, TableReading,
};
use anyhow::Error;
use just_llm_client::types::generation::{
    GenerationRequest, Message, ToolChoice, ToolChoiceMode, ToolDefinition,
};
use kallipai_common::retry::{RetryAttempt, RetryRecord};
use kallipai_common::timefmt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

/// Within-set failover acquisition: rebuild the request per profile, retry, and on a `Failover`
/// outcome advance the chain. Self-contained — owns `retry_records` and flushes them on every
/// early-exit arm and after a successful break.
///
/// On `Failover` (endpoint-level failure, or transient retries exhausted) the acquisition loop
/// advances to the next profile in the set, rebuilds the client, and retries the same turn. On `Fatal`
/// (request-level) it errors the round. `profile_idx` only moves forward and sticks for the
/// agent's lifetime (resets to 0 on spawn/restore). The inner `tokio::select!` cancel arm stays
/// inside this function so a cancel during the retry backoff flushes and short-circuits here.
pub(crate) async fn acquire_stream(
    ctx: &mut AgentContext,
    mut messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    round_cancel: &CancellationToken,
    round: usize,
) -> AcquireResult {
    let mut retry_records = Vec::new();
    let consumed = loop {
        let endpoint_id = ctx.failover.current_profile().endpoint.clone();
        // The endpoint, the clock, the attempt origin and the table's reading are
        // all taken here, so a chain that advanced is seen with fresh values: the
        // verdict on them is `enter_attempt`'s, which owns what a carried fact means.
        let now = timefmt::now_epoch();
        let origin = ctx.take_attempt_origin();
        let reading = table_reading(ctx, now);
        let pre_send = Attempt::pre_send(Phase::Connect, origin, now, reading.clone());
        match enter_attempt(
            ctx,
            &mut messages,
            tx,
            round_cancel,
            round,
            pre_send,
            reading.clone(),
        )
        .await
        {
            // The attempt proceeds to the send.
            PreSend::Send => {}
            PreSend::Advanced => continue,
            PreSend::Done(result) => return result,
        }
        let request = build_request(ctx, &messages, &tools);
        let result = {
            let fut = crate::retry::stream_with_retry(
                crate::retry::RetryCall {
                    conversation: &mut ctx.conversation,
                    request,
                    policy: &ctx.config.retry_policy,
                    round,
                    episode: &mut ctx.episode,
                    endpoint_id: &endpoint_id,
                    health: ctx.endpoint_health.as_deref(),
                },
                tx,
                &mut retry_records,
                round_cancel.clone(),
            );
            tokio::select! {
                result = fut => result,
                _ = round_cancel.cancelled() => {
                    flush_retry_records(ctx, &mut retry_records).await;
                    return AcquireResult::Outcome(AgentOutcome::Cancelled);
                }
            }
        };
        let stream = match result {
            Ok(stream) => {
                // The first successful send that crosses the fact the table
                // carries: how long the account stayed refused, measured from the
                // fact's own observation. Log channel only — the record face
                // carries failures, and this is the recovery side of the same
                // window.
                if let (Some(fact), AttemptOrigin::External) = (reading.blocked.as_ref(), origin)
                    && fact.block.kind.is_account_level()
                {
                    info!(
                        agent = ?ctx.config.agent_id,
                        endpoint = %endpoint_id,
                        since_quota_ts = fact.block.recorded_at,
                        blocked_secs =
                            timefmt::now_epoch().saturating_sub(fact.block.recorded_at),
                        "a send crossed the shared fact the table carries"
                    );
                }
                stream
            }
            Err(crate::retry::RequestFailure::Fatal(e)) => {
                flush_retry_records(ctx, &mut retry_records).await;
                return AcquireResult::Error(e.into());
            }
            Err(crate::retry::RequestFailure::Failover(e)) => {
                // Flush this endpoint's retries (each tagged with its endpoint id) before
                // advancing.
                flush_retry_records(ctx, &mut retry_records).await;
                let reason = crate::llm_error::render_error(&e);
                if let Some(result) = walk_or_finish(
                    ctx,
                    &mut messages,
                    FailedSend {
                        trigger: e.into(),
                        reason,
                        exhaustion: Exhaustion::Transient,
                    },
                    tx,
                    round_cancel,
                    round,
                )
                .await
                {
                    return result;
                }
                continue;
            }
            Err(crate::retry::RequestFailure::Switch {
                reason,
                error,
                block,
            }) => {
                flush_retry_records(ctx, &mut retry_records).await;
                let rendered = crate::llm_error::render_error(&error);
                // A quota or payment block parks with the structured reason when
                // the chain runs out; every other one-way switch keeps its own
                // wording. The criterion is the fact the failure leaves, not the
                // exit it happens to share.
                let exhaustion = match block {
                    Some(kind) if kind.is_account_level() => Exhaustion::QuotaBlocked(
                        quota_blocked_payload(ctx, &endpoint_id, rendered.clone()),
                    ),
                    _ => Exhaustion::ExternalOnly {
                        stop_reason: reason,
                    },
                };
                if let Some(result) = walk_or_finish(
                    ctx,
                    &mut messages,
                    FailedSend {
                        trigger: error.into(),
                        reason: rendered,
                        exhaustion,
                    },
                    tx,
                    round_cancel,
                    round,
                )
                .await
                {
                    return result;
                }
                continue;
            }
            Err(crate::retry::RequestFailure::Stop { reason, error }) => {
                flush_retry_records(ctx, &mut retry_records).await;
                return AcquireResult::Stop {
                    message: stop_message(reason, ctx, &error, LOCAL_CONFIG_NOTE),
                };
            }
            Err(crate::retry::RequestFailure::Cancelled) => {
                // Cancel surfaced from within a retry backoff — flush this endpoint's
                // retries and short-circuit to a cancelled round. Mirrors the Fatal arm's flush.
                flush_retry_records(ctx, &mut retry_records).await;
                return AcquireResult::Outcome(AgentOutcome::Cancelled);
            }
        };

        // Consume the acquired stream. A mid-stream transport drop is retryable in-place:
        // the partial content is abandoned (already-emitted deltas are voided downstream by
        // a `StreamReset` event) and the same request is re-sent; the kept anchor turns the
        // retry into a delta continuation.
        match consume_stream(stream, tx, round_cancel).await {
            StreamOutcome::Completed(mut c) => {
                flush_retry_records(ctx, &mut retry_records).await;
                if let Some(response) = c.response.take()
                    && let Err(error) = ctx.conversation.adopt(*response)
                {
                    // Unreachable in practice: the memo pairs the just-completed
                    // attempt and `into_response` yields a non-empty id. The prior
                    // anchor stays valid server-side, so warn and continue: the next
                    // turn re-sends the messages past the kept anchor.
                    tracing::warn!(
                        "failed to adopt the completed stream: {}",
                        crate::llm_error::render_error(&error)
                    );
                }
                break c;
            }
            StreamOutcome::Cancelled => {
                flush_retry_records(ctx, &mut retry_records).await;
                return AcquireResult::Outcome(AgentOutcome::Cancelled);
            }
            StreamOutcome::Transient(e) => {
                // This attempt's prepare/send retries (if any) were buffered locally; persist
                // them first so the observation log is complete before we decide whether a
                // mid-stream retry remains within the episode's budget.
                flush_retry_records(ctx, &mut retry_records).await;
                if let Some(result) =
                    on_mid_stream_failure(ctx, &mut messages, tx, round_cancel, round, e).await
                {
                    return result;
                }
            }
        }
    };
    AcquireResult::Consumed(consumed)
}

/// What the pre-send verdict asks the driver to do.
enum PreSend {
    /// The attempt proceeds: the driver builds the request and sends it.
    Send,
    /// The chain advanced. The driver runs the loop head again, so the endpoint,
    /// the clock, the attempt origin and the table reading are all taken afresh:
    /// a fact another agent wrote while the walk ran still stops the send.
    Advanced,
    /// The round ends here, with this result.
    Done(AcquireResult),
}

/// Settle the pre-send verdict for one attempt: the episode's ledger, and what the
/// shared table says about the endpoint this send would go to. A fact the table carries
/// means the send does not happen at all — the chain walks, or the round stops — and a
/// spent ledger means the episode already charged its sends elsewhere (a mid-stream
/// re-entry continues the episode), so the attempt starts over rather than erroring
/// the round.
///
/// `PreSend::Send` means the attempt proceeds, `PreSend::Advanced` means the chain
/// advanced and the driver takes the attempt from the top, `PreSend::Done` ends the
/// round with the result.
async fn enter_attempt(
    ctx: &mut AgentContext,
    messages: &mut Vec<Message>,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    round_cancel: &CancellationToken,
    round: usize,
    pre_send: Attempt,
    reading: TableReading,
) -> PreSend {
    let endpoint_id = ctx.failover.current_profile().endpoint.clone();
    let verdict = decision::decide(&ctx.episode.facts(&pre_send), &ctx.config.retry_policy);
    match verdict {
        Decision::Proceed => {}
        // The table's verdict with a candidate: the chain walks with the
        // fact's own reason, and running out of candidates stops the round
        // (no re-run: another attempt would meet the same fact).
        Decision::AdvanceFailover { reason } => {
            log_fact_skip(ctx, &endpoint_id, &reading, reason);
            let fact = fact_error(&reading);
            let rendered = crate::llm_error::render_error(fact.as_ref());
            // An account-level fact means the walk is the quota exit, so
            // running out parks with the structured reason; an endpoint-level
            // one has no account to name and stops with the fact's wording.
            let exhaustion = match reading.blocked.as_ref() {
                Some(blocked) if blocked.block.kind.is_account_level() => Exhaustion::QuotaBlocked(
                    quota_blocked_payload(ctx, &endpoint_id, blocked.block.detail.clone()),
                ),
                _ => Exhaustion::ExternalOnly {
                    stop_reason: reason,
                },
            };
            if let Some(result) = walk_or_finish(
                ctx,
                messages,
                FailedSend {
                    trigger: fact,
                    reason: rendered,
                    exhaustion,
                },
                tx,
                round_cancel,
                round,
            )
            .await
            {
                // The walk is over and the fact is why: the gateway hears
                // about it here, once. A walk that recovered stays silent.
                emit_fact_signal(ctx, reading.blocked.as_ref());
                return PreSend::Done(result);
            }
            // The walk recovered: the driver starts the attempt over against the
            // new endpoint and re-reads the table there.
            return PreSend::Advanced;
        }
        // The table's verdict with nowhere to go: the round stops here, with
        // no request made and the fact as the evidence.
        Decision::Stop {
            kind: StopKind::ExternalOnly,
            reason,
        } => {
            log_fact_skip(ctx, &endpoint_id, &reading, reason);
            if let Some(fact) = reading.blocked.as_ref()
                && fact.block.kind.is_account_level()
            {
                return PreSend::Done(AcquireResult::Outcome(AgentOutcome::QuotaBlocked(
                    quota_blocked_payload(ctx, &endpoint_id, fact.block.detail.clone()),
                )));
            }
            // The round ends on the table's verdict: an endpoint-level auth
            // fact says the same thing a rejection would.
            emit_fact_signal(ctx, reading.blocked.as_ref());
            return PreSend::Done(AcquireResult::Stop {
                message: fact_stop_message(reason, ctx, &reading, fact_chain_note(ctx)),
            });
        }
        // The episode's own budget or deadline ran out. The wording of the
        // error is shared with the retry path's own guard.
        Decision::Stop {
            kind: StopKind::ChainExhausted,
            reason,
        } => {
            error!(
                attempts = ctx.episode.attempts(),
                max_attempts = ctx.config.retry_policy.max_retries,
                endpoint = %endpoint_id,
                reason,
                "episode retry budget exhausted, advancing failover chain"
            );
            let spent = crate::retry::spent_budget_error();
            let rendered = crate::llm_error::render_error(&spent);
            if let Some(result) = walk_or_finish(
                ctx,
                messages,
                FailedSend {
                    trigger: spent.into(),
                    reason: rendered,
                    exhaustion: Exhaustion::Transient,
                },
                tx,
                round_cancel,
                round,
            )
            .await
            {
                return PreSend::Done(result);
            }
            // The walk advanced: the driver starts the attempt over against the
            // new endpoint.
            return PreSend::Advanced;
        }
        // A pre-send reading cannot fail the round on its own: the arm keeps
        // the match exhaustive, and a round error beats a panic.
        other => {
            error!(verdict = ?other, "pre-send reading produced no performable verdict");
            return PreSend::Done(AcquireResult::Error(anyhow::anyhow!(
                "pre-send reading produced a verdict this path cannot perform"
            )));
        }
    }
    PreSend::Send
}

/// Rebuild this attempt's request from the failover state. Request construction is
/// call-side: the upstream `Conversation` has no `create_request`, so model and system
/// prompt are injected here from the failover state (the summarizer keeps its own
/// client-level construction). Effort rides the current profile and is re-read every
/// round: a failover that swaps profiles must swap the requested effort with it.
fn build_request(
    ctx: &AgentContext,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> GenerationRequest {
    let mut request = GenerationRequest::new(
        ctx.failover.current_profile().model.clone(),
        messages.to_vec(),
    )
    .with_tools(tools.to_vec())
    .with_tool_choice(ToolChoice::Mode(ToolChoiceMode::Auto));
    if let Some(effort) = ctx.failover.current_profile().effort.clone() {
        request = request.with_reasoning_effort(effort);
    }
    if let Some(prompt) = ctx.failover.system_prompt() {
        request = request.with_system_prompt(prompt);
    }
    request.stream = Some(true);
    request
}

/// Decide a mid-stream transport drop: promote it to a provider error, classify it in
/// the phase it happened in, and read the exit from the episode ledger — stop the round,
/// walk the chain with the partial downstream voided, or back off and re-acquire in place.
///
/// `None` means the loop re-acquires; `Some` ends the round here.
async fn on_mid_stream_failure(
    ctx: &mut AgentContext,
    messages: &mut Vec<Message>,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    round_cancel: &CancellationToken,
    round: usize,
    e: just_llm_client::TransportError,
) -> Option<AcquireResult> {
    let endpoint_id = ctx.failover.current_profile().endpoint.clone();
    let policy = &ctx.config.retry_policy;
    let attempt = ctx.episode.attempts();
    let error_msg = crate::llm_error::render_error(&e);

    // A mid-stream failure is decided by the same table as a prepare/send
    // failure: promote it to a provider error, classify it in the phase it
    // happened in, and read the exit from the episode ledger. A repeated
    // response shape stops the round here just as it does before the stream
    // opens: the class picks the exit, not the transport nature of the error.
    let promoted = just_llm_client::BackendError::provider(ctx.client.family(), e);
    let class = decision::classify(&promoted, Phase::MidStream);
    let attempt_facts = Attempt::failed(
        Phase::MidStream,
        class,
        decision::shape_key(&promoted),
        timefmt::now_epoch(),
        crate::retry::jitter_now(),
    );

    let verdict = decision::decide(&ctx.episode.facts(&attempt_facts), policy);
    match verdict {
        // A local misconfiguration fails on every profile, so the chain
        // cannot help: stop the round here, as the send path does.
        Decision::Stop {
            kind: StopKind::ExternalOnly,
            reason,
        } => {
            return Some(AcquireResult::Stop {
                message: stop_message(reason, ctx, &promoted, LOCAL_CONFIG_NOTE),
            });
        }
        // A class another send cannot help with, but a fresh endpoint can:
        // void the partial downstream with no wait, then let the chain walk
        // decide: a candidate left means a fresh endpoint, an exhausted
        // chain stops the round.
        Decision::AdvanceFailover { reason } => {
            error!(
                attempt,
                max_attempts = policy.max_retries,
                error = %error_msg,
                reason,
                "mid-stream failure is not retryable, advancing failover chain"
            );
            void_partial(tx, error_msg, attempt, policy.max_retries, 0.0);
            let rendered = crate::llm_error::render_error(&promoted);
            // What this failure taught about the endpoint or the account
            // outlives the round, so it goes to the shared table before the
            // chain walks.
            crate::retry::record_endpoint_fact(
                ctx.endpoint_health.as_deref(),
                &endpoint_id,
                class,
                &promoted,
                timefmt::now_epoch(),
            );
            // The class decides the carrier, exactly as it does on the
            // send path: a quota or payment block parks with the
            // structured reason, an endpoint-level one keeps its wording.
            let exhaustion = if class == decision::FailureClass::QuotaBlocked {
                Exhaustion::QuotaBlocked(quota_blocked_payload(ctx, &endpoint_id, rendered.clone()))
            } else {
                Exhaustion::ExternalOnly {
                    stop_reason: reason,
                }
            };
            // A quota or payment rejection leaves one observation
            // record, exactly as the send path does: the same class
            // criterion decides it, the kind comes from the one
            // mapping, and no wait follows (there is no retry to hang
            // it on). Without it, a mid-stream rejection would be
            // invisible to the aggregation surfaces.
            let now = timefmt::now_epoch();
            let block = decision::fact_of(class, &promoted, now);
            if block
                .as_ref()
                .is_some_and(|block| block.kind.is_account_level())
                && let Some(kind) = decision::retry_kind(&class)
            {
                ctx.store.lock().await.record_retries(
                    [RetryRecord::observed(
                        RetryAttempt {
                            round,
                            attempt,
                            max_attempts: policy.max_retries,
                        },
                        now,
                        Some(endpoint_id.clone()),
                        kind,
                        crate::llm_error::render_error(&promoted),
                        block.as_ref().and_then(|block| block.until),
                    )],
                    retry_keep_since(ctx),
                );
                ctx.persist().await;
            }
            if let Some(result) = walk_or_finish(
                ctx,
                messages,
                FailedSend {
                    trigger: promoted.into(),
                    reason: rendered,
                    exhaustion,
                },
                tx,
                round_cancel,
                round,
            )
            .await
            {
                return Some(result);
            }
        }
        // A verdict that ends the round: the pre-send reading cannot
        // appear past a send, and a request-level permanent failure
        // fails on every profile. The caller's image degrade (if any)
        // sees the promoted error.
        Decision::Proceed | Decision::Fatal { .. } => {
            debug!(
                verdict = ?verdict,
                error = %error_msg,
                "mid-stream failure fails the round"
            );
            return Some(AcquireResult::Error(promoted.into()));
        }
        // Budget or deadline spent: the endpoint keeps dropping mid-stream.
        // Void the partial downstream (no backoff: failover is immediate;
        // `delay_secs: 0.0`), then treat as transient-exhausted and advance
        // the failover chain (a different profile/endpoint may hold a
        // healthier connection). `attempt` is this episode's send count, so
        // it reads `max_attempts` or above when spent.
        Decision::Stop {
            kind: StopKind::ChainExhausted,
            reason,
        } => {
            error!(
                attempt,
                max_attempts = policy.max_retries,
                error = %error_msg,
                reason,
                "mid-stream retry budget exhausted, advancing failover chain"
            );
            void_partial(tx, error_msg, attempt, policy.max_retries, 0.0);
            let rendered = crate::llm_error::render_error(&promoted);
            if let Some(result) = walk_or_finish(
                ctx,
                messages,
                FailedSend {
                    trigger: promoted.into(),
                    reason: rendered,
                    exhaustion: Exhaustion::Transient,
                },
                tx,
                round_cancel,
                round,
            )
            .await
            {
                return Some(result);
            }
        }
        // Within budget: tell downstream to fold/discard the partial it
        // already rendered, back off, record the retry, then re-acquire.
        // Mirrors the emit/record shape of `retry::stream_with_retry`'s retry
        // arm. The server's `Retry-After` header never reaches this layer, so
        // there is no floor to apply. Budget,
        // deadline and backoff all come from the episode ledger, so a
        // mid-stream drop spends the same allowance as a prepare/send retry
        // and the wait is clamped to what is left of `retry_timeout` — by the
        // core, so no caller can sleep past it. The backoff exponent is the
        // sends already made in this episode, so flakiness lengthens the wait.
        Decision::Retry { after } => {
            let Some(kind) = decision::retry_kind(&class) else {
                // A retryable class always maps to a record kind; failing
                // the round beats recording a retry under a made-up one.
                return Some(AcquireResult::Error(promoted.into()));
            };
            // A shape-class retry spends this episode's one shape slot for that
            // key, so a repeat of the same shape exits as endpoint-level.
            crate::retry::spend_shape_slot(&mut ctx.episode, &promoted);
            let delay_secs = after.as_secs_f64();
            void_partial(
                tx,
                error_msg.clone(),
                attempt,
                policy.max_retries,
                delay_secs,
            );
            let record = RetryRecord::retry(
                RetryAttempt {
                    round,
                    attempt,
                    max_attempts: policy.max_retries,
                },
                timefmt::now_epoch(),
                Some(endpoint_id.clone()),
                kind,
                error_msg,
                delay_secs,
            );
            tokio::select! {
                _ = tokio::time::sleep(after) => {
                    ctx.store.lock().await.record_retries([record], retry_keep_since(ctx));
                    ctx.persist().await;
                }
                _ = round_cancel.cancelled() => {
                    return Some(AcquireResult::Outcome(AgentOutcome::Cancelled));
                }
            }
            // Loop continues; the episode ledger already counts the send that just failed,
            // and the re-sent request charges another one against the same budget.
        }
    }
    None
}

/// Walk the failover chain with this failure, or finish the round. `None` means the chain
/// advanced — the caller re-loops with the rebound `messages`; `Some` carries the terminal
/// result.
async fn walk_or_finish(
    ctx: &mut AgentContext,
    messages: &mut Vec<Message>,
    failed: FailedSend,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    round_cancel: &CancellationToken,
    round: usize,
) -> Option<AcquireResult> {
    match step_failover(ctx, messages, failed, tx, round_cancel, round).await {
        FailoverStep::Advanced => None,
        FailoverStep::Done(result) => Some(result),
    }
}

/// Void the partial stream downstream before the next attempt: a `StreamReset` tells the
/// consumers to fold or discard what they already rendered. `delay_secs` is the backoff the
/// caller is about to wait, so the reset carries it.
fn void_partial(
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    error: String,
    attempt: u32,
    max_attempts: u32,
    delay_secs: f64,
) {
    tx.try_send(AgentEvent::StreamReset {
        error,
        attempt,
        max_attempts,
        delay_secs,
    })
    .ok();
}

/// The deterministic image-rejection fallback: the provider rejected the
/// request with a 4xx while the composed request carried images. Strip
/// the images from a recomposed copy (the store stays untouched until
/// the retry proves the images were the problem), retry once, and on
/// success apply the strip for good, plus a plain-language note turn so
/// the agent knows its images were dropped. The strip lives in the
/// store only: invalidations are not remembered across restarts, so a
/// later restore re-fetches the references and degrades again if gone.
///
/// Returns `Some(result)` when the degrade path owned the outcome (the
/// retried stream, or a cancel/budget outcome from the retry); `None`
/// hands the original error back to the caller — the retry failed too,
/// so the images were probably not the cause, and nothing was
/// invalidated.
///
/// Attribution granularity is the turn: the provider blames the request,
/// not an individual part. Every ingest turn carries exactly one image
/// today, so turn granularity equals reference granularity; multi-image
/// turns would need finer attribution.
pub(crate) async fn degrade_on_image_rejection(
    ctx: &mut AgentContext,
    error: &Error,
    tools: Vec<ToolDefinition>,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    round_cancel: &CancellationToken,
    round: usize,
) -> Option<AcquireResult> {
    let status = fatal_provider_status(error)?;

    // Which live turns carry images? None → the rejection is not about
    // images; hand the error back untouched.
    let image_turn_ids: Vec<u64> = {
        let guard = ctx.store.lock().await;
        guard
            .turns()
            .iter()
            .filter(|t| t.messages.iter().any(message_has_images))
            .map(|t| t.id.0)
            .collect()
    };
    if image_turn_ids.is_empty() {
        return None;
    }

    // Recompose (the store has not moved since the failed compose) and
    // retry once without the images.
    let composed = compose_context(ctx.store.clone()).await;
    let stripped: Vec<Message> = composed
        .iter()
        .map(|m| strip_message_images(m).unwrap_or_else(|| m.clone()))
        .collect();
    // The strip-and-resend is a new request, not a retry of the rejected one:
    // it starts its own episode, so a spent retry budget cannot starve it.
    ctx.start_episode();
    match acquire_stream(ctx, stripped, tools, tx, round_cancel, round).await {
        AcquireResult::Consumed(consumed) => {
            // The images were the problem: make the strip permanent.
            {
                let mut guard = ctx.store.lock().await;
                for turn in guard.turns_mut() {
                    if !image_turn_ids.contains(&turn.id.0) {
                        continue;
                    }
                    for message in &mut turn.messages {
                        if let Some(stripped) = strip_message_images(message) {
                            *message = stripped;
                        }
                    }
                    turn.estimated_tokens = crate::context::Turn::estimate_tokens(&turn.messages);
                }
                guard.mark_needs_full_estimate();
            }

            // Tell the agent, in plain text, what happened to its images.
            let note = format!(
                "[system] image attachments were removed from the request: the provider \
                 rejected them (HTTP {status}). the affected turns stay text-only; \
                 continue without the image content."
            );
            ctx.record_turn(vec![Message::user(&note)]).await;
            Some(AcquireResult::Consumed(consumed))
        }
        AcquireResult::Outcome(outcome) => Some(AcquireResult::Outcome(outcome)),
        AcquireResult::Error(_) => None,
        // A stop is already a verdict about the chain, not about the images:
        // propagate it instead of degrading and resending.
        AcquireResult::Stop { message } => Some(AcquireResult::Stop { message }),
    }
}
