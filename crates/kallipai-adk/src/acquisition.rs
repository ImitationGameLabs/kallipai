//! LLM stream acquisition for the agent round loop.
//!
//! Owns the within-set acquisition loop: consume an SSE stream, retry mid-stream
//! transport drops in place, and advance the failover chain when an endpoint or the
//! retry budget gives out. The [`crate::failover::FailoverState`] state machine stays
//! pure in its own module — this module drives it, and `advance_failover` below is the
//! sole driver of chain advancement. The round loop in `crate::runner` calls
//! [`acquire_stream`] and dispatches on the result.

use anyhow::Error;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::agent_task::AgentContext;
use crate::context::{
    CompactOutcome, compose_context, message_has_images, strip_message_images, summarize_and_evict,
};
use crate::event::{AgentEvent, AgentOutcome};
use crate::failover::FailoverOutcome;
use crate::retry::decision::{
    self, Alternative, Attempt, AttemptOrigin, Decision, Phase, StopKind, TableReading,
};
use crate::stream_accumulator::ToolCallAccumulator;
use just_llm_client::types::generation::{
    GenerationEvent, GenerationRequest, Message, ToolCall, ToolChoice, ToolChoiceMode,
    ToolDefinition,
};
use just_llm_client::{BackendError, ConversationStream, provider_rejection};
use kallipai_common::facts::{BlockKind, BlockedFact, EndpointHealth, display_until};
use kallipai_common::protocol::{FailoverChainExhaustion, QuotaBlockedInfo};
use kallipai_common::retry::RetryRecord;
use kallipai_common::timefmt;

// ---------------------------------------------------------------------------
// Stream consumption
// ---------------------------------------------------------------------------

/// Outcome of consuming an LLM response stream.
enum StreamOutcome {
    /// The stream was cancelled mid-stream.
    Cancelled,
    /// The stream completed normally.
    Completed(StreamConsumed),
    /// The stream dropped mid-way with a transport error. The partial content accumulated so far
    /// is abandoned (already-emitted deltas are void); the caller re-sends the same request,
    /// which the kept anchor turns into a delta continuation. Carries the error so the caller
    /// can drive retry/failover and surface a diagnostic.
    Transient(just_llm_client::TransportError),
}

/// Data accumulated from a completed LLM response stream.
pub(crate) struct StreamConsumed {
    pub(crate) content: String,
    pub(crate) reasoning: String,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) usage: Option<just_llm_client::types::generation::Usage>,
    /// The assembled response of a stream that reached its `End` event carrying a response
    /// id — the adoptable turn record. `None` when the backend is stateless (no id on
    /// `End`) or the stream never completed; the caller adopts only when this is `Some`.
    /// Boxed so this struct stays small next to the variants that carry it.
    pub(crate) response: Option<Box<just_llm_client::types::generation::GenerationResponse>>,
}

/// Consume an SSE stream, accumulating content, reasoning, tool calls, and usage.
///
/// Takes ownership of the stream; the assembled response rides the `Completed` outcome.
/// Returns `Cancelled` if the cancellation token fires mid-stream.
async fn consume_stream(
    mut stream: ConversationStream,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    cancel: &CancellationToken,
) -> StreamOutcome {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_acc = ToolCallAccumulator::new();
    let mut response_usage: Option<just_llm_client::types::generation::Usage> = None;

    loop {
        tokio::select! {
            event_result = stream.next() => {
                let event = match event_result {
                    Some(Ok(e)) => e,
                    Some(Err(e)) => {
                        // Mid-stream transport drop (connection reset, h2 error, premature EOF, ...).
                        // The partial content already emitted via deltas is void; the caller re-sends
                        // the same request and emits a `StreamReset` so downstream folds/discards it.
                        warn!(
                            error = %crate::llm_error::render_error(&e),
                            "LLM stream dropped mid-stream"
                        );
                        return StreamOutcome::Transient(e);
                    }
                    None => break,
                };
                match event {
                    GenerationEvent::Text { delta } => {
                        content.push_str(&delta);
                        tx.send(AgentEvent::AssistantContentDelta { delta })
                            .await
                            .ok();
                    }
                    GenerationEvent::Reasoning { delta } => {
                        reasoning.push_str(&delta);
                        tx.send(AgentEvent::ReasoningDelta { delta }).await.ok();
                    }
                    GenerationEvent::ToolCall { delta } => {
                        tool_acc.push(&delta);
                    }
                    GenerationEvent::Usage { usage } => {
                        response_usage = Some(usage);
                    }
                    // Stream close, not `End`, terminates the loop; `End` only carries the
                    // finish reason / response id, which the round loop does not consume yet.
                    GenerationEvent::End { .. } => {}
                }
            }
            _ = cancel.cancelled() => {
                tracing::info!("LLM stream cancelled mid-stream");
                return StreamOutcome::Cancelled;
            }
        }
    }

    // The stream was consumed through its terminal `End` event: take the assembled
    // response for the caller to adopt. `None` = stateless backend or no id — nothing to adopt.
    let response = stream.into_response().map(Box::new);
    StreamOutcome::Completed(StreamConsumed {
        content,
        reasoning,
        tool_calls: tool_acc.finish(),
        usage: response_usage,
        response,
    })
}

// ---------------------------------------------------------------------------
// Within-set acquisition
// ---------------------------------------------------------------------------

/// Outcome of the within-set failover acquisition loop.
pub(crate) enum AcquireResult {
    /// A stream was acquired AND fully consumed — proceed to post-stream budgeting / tool calls.
    Consumed(StreamConsumed),
    /// A terminal round outcome (chain exhausted / cancelled / budget exceeded).
    Outcome(AgentOutcome),
    /// A request-level error — the round errors.
    Error(Error),
    /// The chain cannot help: stop the round with this message (no candidate
    /// left, or a local misconfiguration). Crate-internal — it never reaches
    /// the wire.
    Stop { message: String },
}

/// The provider HTTP status behind a request-level acquisition error, if
/// any: a `400`/`422`-family rejection that failed on this profile.
/// Auth/404 statuses never reach the Fatal arm (they fail over instead)
/// and pre-flight serialization failures carry no status — both return
/// `None`, so the caller's image-degrade path only fires on true
/// provider rejections.
pub(crate) fn fatal_provider_status(error: &Error) -> Option<reqwest::StatusCode> {
    let backend = error.downcast_ref::<BackendError>()?;
    provider_rejection(backend)
        .map(|rejection| rejection.status)
        .filter(|status| {
            status.is_client_error()
                && *status != reqwest::StatusCode::UNAUTHORIZED
                && *status != reqwest::StatusCode::FORBIDDEN
                && *status != reqwest::StatusCode::NOT_FOUND
        })
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
        // Before a send: the core's verdict on the episode's ledger and on what
        // the shared table says about the endpoint this send would go to. A fact
        // the table carries means the send does not happen at all — the chain
        // walks, or the round stops — and a spent ledger means the episode
        // already charged its sends elsewhere (a mid-stream re-entry continues
        // the episode), so the loop advances rather than erroring the round.
        let now = timefmt::now_epoch();
        let origin = ctx.take_attempt_origin();
        let reading = table_reading(ctx, now);
        let pre_send = Attempt::pre_send(Phase::Connect, origin, now, reading.clone());
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
                    Some(blocked) if blocked.block.kind.is_account_level() => {
                        Exhaustion::QuotaBlocked(quota_blocked_payload(
                            ctx,
                            &endpoint_id,
                            blocked.block.detail.clone(),
                        ))
                    }
                    _ => Exhaustion::ExternalOnly {
                        stop_reason: reason,
                    },
                };
                let step = step_failover(
                    ctx,
                    &mut messages,
                    fact,
                    rendered,
                    exhaustion,
                    tx,
                    round_cancel,
                )
                .await;
                match step {
                    FailoverStep::Advanced => continue,
                    // The walk is over and the fact is why: the gateway hears
                    // about it here, once. A walk that recovered stays silent.
                    FailoverStep::Done(result) => {
                        emit_fact_signal(ctx, reading.blocked.as_ref());
                        return result;
                    }
                }
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
                    return AcquireResult::Outcome(AgentOutcome::QuotaBlocked(
                        quota_blocked_payload(ctx, &endpoint_id, fact.block.detail.clone()),
                    ));
                }
                // The round ends on the table's verdict: an endpoint-level auth
                // fact says the same thing a rejection would.
                emit_fact_signal(ctx, reading.blocked.as_ref());
                return AcquireResult::Stop {
                    message: fact_stop_message(reason, ctx, &reading, fact_chain_note(ctx)),
                };
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
                let step = step_failover(
                    ctx,
                    &mut messages,
                    spent.into(),
                    rendered,
                    Exhaustion::Transient,
                    tx,
                    round_cancel,
                )
                .await;
                match step {
                    FailoverStep::Advanced => continue,
                    FailoverStep::Done(result) => return result,
                }
            }
            // A pre-send reading cannot fail the round on its own: the arm keeps
            // the match exhaustive, and a round error beats a panic.
            other => {
                error!(verdict = ?other, "pre-send reading produced no performable verdict");
                return AcquireResult::Error(anyhow::anyhow!(
                    "pre-send reading produced a verdict this path cannot perform"
                ));
            }
        }

        // Request construction is call-side: the upstream `Conversation` has no
        // `create_request`, so model and system prompt are injected here from the
        // failover state (the summarizer keeps its own client-level construction).
        let mut request = GenerationRequest::new(
            ctx.failover.current_profile().model.clone(),
            messages.clone(),
        )
        .with_tools(tools.clone())
        .with_tool_choice(ToolChoice::Mode(ToolChoiceMode::Auto));
        // Effort rides the current profile and is re-read every round: a failover that
        // swaps profiles must swap the requested effort with it.
        if let Some(effort) = ctx.failover.current_profile().effort.clone() {
            request = request.with_reasoning_effort(effort);
        }
        if let Some(prompt) = ctx.failover.system_prompt() {
            request = request.with_system_prompt(prompt);
        }
        request.stream = Some(true);
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
                let step = step_failover(
                    ctx,
                    &mut messages,
                    e.into(),
                    reason,
                    Exhaustion::Transient,
                    tx,
                    round_cancel,
                )
                .await;
                match step {
                    FailoverStep::Advanced => continue,
                    FailoverStep::Done(result) => return result,
                }
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
                let step = step_failover(
                    ctx,
                    &mut messages,
                    error.into(),
                    rendered,
                    exhaustion,
                    tx,
                    round_cancel,
                )
                .await;
                match step {
                    FailoverStep::Advanced => continue,
                    FailoverStep::Done(result) => return result,
                }
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
                        return AcquireResult::Stop {
                            message: stop_message(reason, ctx, &promoted, LOCAL_CONFIG_NOTE),
                        };
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
                        tx.try_send(AgentEvent::StreamReset {
                            error: error_msg,
                            attempt,
                            max_attempts: policy.max_retries,
                            delay_secs: 0.0,
                        })
                        .ok();
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
                            Exhaustion::QuotaBlocked(quota_blocked_payload(
                                ctx,
                                &endpoint_id,
                                rendered.clone(),
                            ))
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
                                [RetryRecord {
                                    timestamp: now,
                                    round,
                                    attempt,
                                    max_attempts: policy.max_retries,
                                    error: crate::llm_error::render_error(&promoted),
                                    delay_secs: 0.0,
                                    endpoint: Some(endpoint_id.clone()),
                                    kind,
                                    quota_reset: block.as_ref().and_then(|block| block.until),
                                }],
                                retry_keep_since(ctx),
                            );
                            ctx.persist().await;
                        }
                        match step_failover(
                            ctx,
                            &mut messages,
                            promoted.into(),
                            rendered,
                            exhaustion,
                            tx,
                            round_cancel,
                        )
                        .await
                        {
                            FailoverStep::Advanced => continue,
                            FailoverStep::Done(result) => return result,
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
                        return AcquireResult::Error(promoted.into());
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
                        tx.try_send(AgentEvent::StreamReset {
                            error: error_msg,
                            attempt,
                            max_attempts: policy.max_retries,
                            delay_secs: 0.0,
                        })
                        .ok();
                        let rendered = crate::llm_error::render_error(&promoted);
                        match step_failover(
                            ctx,
                            &mut messages,
                            promoted.into(),
                            rendered,
                            Exhaustion::Transient,
                            tx,
                            round_cancel,
                        )
                        .await
                        {
                            FailoverStep::Advanced => continue,
                            FailoverStep::Done(result) => return result,
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
                            return AcquireResult::Error(promoted.into());
                        };
                        // A shape-class retry spends this episode's one shape slot for that
                        // key, so a repeat of the same shape exits as endpoint-level.
                        crate::retry::spend_shape_slot(&mut ctx.episode, &promoted);
                        let delay_secs = after.as_secs_f64();
                        tx.try_send(AgentEvent::StreamReset {
                            error: error_msg.clone(),
                            attempt,
                            max_attempts: policy.max_retries,
                            delay_secs,
                        })
                        .ok();
                        let record = RetryRecord {
                            timestamp: timefmt::now_epoch(),
                            round,
                            attempt,
                            max_attempts: policy.max_retries,
                            error: error_msg,
                            delay_secs,
                            endpoint: Some(endpoint_id.clone()),
                            kind,
                            quota_reset: None,
                        };
                        tokio::select! {
                            _ = tokio::time::sleep(after) => {
                                ctx.store.lock().await.record_retries([record], retry_keep_since(ctx));
                                ctx.persist().await;
                            }
                            _ = round_cancel.cancelled() => {
                                return AcquireResult::Outcome(AgentOutcome::Cancelled);
                            }
                        }
                        // Loop continues; the episode ledger already counts the send that just failed,
                        // and the re-sent request charges another one against the same budget.
                    }
                }
            }
        }
    };
    AcquireResult::Consumed(consumed)
}

/// Unix-seconds cutoff for retry-log retention: records older than the retry
/// window are pruned on append. The window is a log-retention policy only:
/// the episode ledger, not the log, is what bounds retries.
fn retry_keep_since(ctx: &AgentContext) -> u64 {
    timefmt::now_epoch().saturating_sub(ctx.config.retry_policy.retry_timeout.as_secs())
}

/// Persist any buffered retry records for this endpoint into the store. A no-op when empty.
async fn flush_retry_records(ctx: &mut AgentContext, retry_records: &mut Vec<RetryRecord>) {
    if retry_records.is_empty() {
        return;
    }
    ctx.store
        .lock()
        .await
        .record_retries(std::mem::take(retry_records), retry_keep_since(ctx));
    ctx.persist().await;
}

// ---------------------------------------------------------------------------
// Failover driving
// ---------------------------------------------------------------------------

/// One step of within-set failover, shared by the prepare/send/parse `Failover` arm and the
/// mid-stream-drop budget-exhausted path so both entry points behave identically.
enum FailoverStep {
    /// The chain advanced to a new profile; the caller re-loops with the rebound `messages`.
    Advanced,
    /// A terminal acquire result (chain exhausted / cancelled / budget exceeded).
    Done(AcquireResult),
}

/// A gateway-level auth rejection (401/403) observed where a round ends on one:
/// at failover-chain exhaustion, or at a stop the shared table's verdict caused.
/// Lean by design: the tagma renders its own operator log and drives
/// profile-source refresh and poison state from status + code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewaySignal {
    pub status: u16,
    pub code: Option<String>,
}

/// The payload of a gateway signal: the status the upstream answered and its own
/// error code, when the response carried one.
type SignalPayload = (u16, Option<String>);

/// The payload a settled failure carries, when it is an upstream rejection at
/// all. The error path's half of the signal: the trigger's own rejection, walked
/// out of the error chain.
fn rejection_payload(trigger: &Error) -> Option<SignalPayload> {
    let backend = trigger.downcast_ref::<BackendError>()?;
    let rejection = provider_rejection(backend)?;
    Some((rejection.status.as_u16(), rejection.code.clone()))
}

/// Fire the gateway's signal for a rejection it acts on — 401/403, the statuses
/// that drive the tagma's profile-source refresh. One round emits at most one:
/// the error path fires when a chain walk exhausts on an auth rejection, and the
/// fact path when the shared table's verdict ends the round on one; a rejection
/// the chain recovers from stays silent. The gate lives here, so both paths hand
/// their payload to one decision. A `None` channel (local profiles, test
/// contexts) is zero behavior.
fn emit_gateway_signal(ctx: &AgentContext, payload: SignalPayload) {
    let (status, code) = payload;
    if status != reqwest::StatusCode::UNAUTHORIZED.as_u16()
        && status != reqwest::StatusCode::FORBIDDEN.as_u16()
    {
        return;
    }
    let Some(signals) = &ctx.gateway_signals else {
        return;
    };
    let _ = signals.send(GatewaySignal { status, code });
}

/// Signal the gateway about the fact the table carried, when it is the kind that
/// says this endpoint's credentials were rejected. An account-level quota or
/// payment block parks the round instead, and a shape fact is not an auth answer;
/// the status gate itself stays in the emitter. A fact stores no error code, so
/// the signal carries none.
fn emit_fact_signal(ctx: &AgentContext, blocked: Option<&BlockedFact>) {
    let Some(block) = blocked.map(|fact| &fact.block) else {
        return;
    };
    if block.kind != BlockKind::EndpointAuth {
        return;
    }
    let Some(status) = block.status else {
        return;
    };
    emit_gateway_signal(ctx, (status, None));
}

/// What a chain exhaustion means for the trigger that caused it — why the chain
/// was being walked at all, and which carrier the round ends with.
///
/// The carrier is chosen where the class is known (the caller), so the mapping
/// from cause to exit is readable at the call site; this enum only carries it
/// down to the exhaustion arm.
#[derive(Clone, Debug)]
enum Exhaustion {
    /// The whole chain is transiently down: the outer re-run budget decides
    /// whether the round is retried later.
    Transient,
    /// A class another send cannot help with, and no payload to carry:
    /// exhaustion stops the round with the class's own wording.
    ExternalOnly { stop_reason: &'static str },
    /// A quota or payment block: exhaustion parks the agent with the structured
    /// reason — which endpoint and account, the moment the upstream said service
    /// resumes, the evidence — and arms nothing.
    QuotaBlocked(QuotaBlockedInfo),
}

/// Why a local configuration failure stops without walking the chain: it fails
/// on every profile, so a switch cannot help.
const LOCAL_CONFIG_NOTE: &str =
    "no failover attempted: a local misconfiguration fails on every profile";

/// The operator-facing message for a stop no payload carries: which endpoint and
/// profile stopped serving, why the chain did not take over, and the rendered
/// evidence.
///
/// No structured carrier exists for these stops (a quota or payment block parks
/// with `QuotaBlockedInfo` instead), so this message is the whole payload. The
/// classes it serves have no resumption moment to report — the one that does
/// carries it in the payload.
fn stop_message(
    stop_reason: &'static str,
    ctx: &AgentContext,
    error: &(dyn std::error::Error + 'static),
    chain_note: &str,
) -> String {
    let profile = ctx.failover.current_profile();
    format!(
        "{}: endpoint {} (profile {}); {}; evidence: {}",
        stop_reason,
        profile.endpoint,
        profile.id,
        chain_note,
        crate::llm_error::render_error(error)
    )
}
/// What the shared table says about the endpoint the next send would go to: the
/// fact it carries, and whether the chain has a candidate to offer instead.
///
/// Fail-open at every step: no handle, an unknown provider, a table that cannot
/// be read — each reads as no fact, because a table that cannot be read must
/// never withhold a send.
fn table_reading(ctx: &AgentContext, now: u64) -> TableReading {
    let Some(health) = ctx.endpoint_health.as_deref() else {
        return TableReading::none();
    };
    let current = ctx.failover.current_profile().endpoint.clone();
    let blocked = health.blocked_for(&current, now);
    // With no fact there is nothing to escape, so no candidate is needed.
    let alternative = blocked.as_ref().and_then(|_| resolve_alternative(ctx, now));
    TableReading {
        blocked,
        alternative,
    }
}

/// The first endpoint ahead of the active profile that could serve where the
/// current one cannot: one the shared table has nothing to say about.
///
/// The walk skips a marked candidate rather than stopping at it, so a marked
/// sibling does not hide a usable endpoint behind it. `None` means the chain has
/// nowhere to go.
///
/// The fact's level decides what "marked" means without a second rule here: the
/// table keys an account-level fact by the credential, so it answers for every
/// endpoint on that account (see [`EndpointHealth::blocked_for`]), which is what
/// leaves a same-account sibling no candidate for escaping a quota block — while
/// an endpoint-level fact keys by endpoint, so a sibling on the same account
/// stays one.
fn resolve_alternative(ctx: &AgentContext, now: u64) -> Option<Alternative> {
    let health = ctx.endpoint_health.as_deref()?;
    let mut offset = 1;
    while let Some(candidate) = ctx.failover.candidate_profile(offset) {
        if !candidate_marked(health, &candidate.endpoint, now) {
            return Some(Alternative);
        }
        offset += 1;
    }
    None
}

/// Whether the shared table rules this candidate out: it carries a fact of its
/// own — of either level, since an account-level fact answers for every endpoint
/// on the credential.
///
/// Fail-open: an unknown provider, or a handle that cannot read, reads as a
/// candidate that may serve.
fn candidate_marked(health: &dyn EndpointHealth, candidate: &str, now: u64) -> bool {
    health.blocked_for(candidate, now).is_some()
}

/// The structured reason a quota or payment block parks with: which endpoint and
/// account, the moment the upstream said service resumes, and the evidence.
///
/// The table is read for the account hint, fail-open: a context without a table,
/// or an endpoint the table cannot key, leaves the hint empty rather than
/// withholding the park. `reset` carries the *display* face of the upstream's
/// hint ([`display_until`]), so the trust rule is applied once, here, and every
/// reader only has to tell `Some` from `None`.
fn quota_blocked_payload(
    ctx: &AgentContext,
    endpoint_id: &str,
    detail: String,
) -> QuotaBlockedInfo {
    let fact = ctx
        .endpoint_health
        .as_deref()
        .and_then(|health| health.blocked_for(endpoint_id, timefmt::now_epoch()));
    let (account_hint, reset) = match fact {
        Some(fact) => (
            fact.account_hint,
            display_until(fact.block.recorded_at, fact.block.until),
        ),
        None => (String::new(), None),
    };
    QuotaBlockedInfo {
        endpoint: endpoint_id.to_string(),
        account_hint,
        reset,
        detail,
    }
}

/// The resumption sentence the fact's diagnostic readers carry: the moment the
/// upstream named for the endpoint to reopen, or that it named none. The
/// failover log line and the stop message say the same thing about the same
/// fact; the trust bound is the display faces' business, applied once where a
/// value becomes a promise a reader may show ([`display_until`]).
fn resumption_note(until: Option<u64>) -> String {
    match until {
        Some(until) => format!("upstream said it reopens at {until} (unix seconds)"),
        None => "the upstream gave no resumption time".to_owned(),
    }
}

/// The error a fact hit carries into the chain walk: no request was made, so the
/// fact is the evidence — its own words, and the moment the upstream said the
/// endpoint reopens. Shaped like `spent_budget_error`: one value carries the
/// wording to every reader (the failover log line, the stop message).
fn fact_error(reading: &TableReading) -> anyhow::Error {
    let Some(fact) = reading.blocked.as_ref() else {
        return anyhow::anyhow!("shared fact: the table has nothing to say");
    };
    let resumption = resumption_note(fact.block.until);
    anyhow::anyhow!(
        "shared fact {:?} on this endpoint: {resumption}; evidence: {}",
        fact.block.kind,
        fact.block.detail
    )
}

/// Why a fact hit stops without walking the chain: there is no endpoint left to
/// try. Three situations produce that, and the note names the one that applies —
/// a set with nothing to walk, a chain already at its last profile, or a chain
/// whose candidates are all marked in the shared table (or share the blocked
/// account).
fn fact_chain_note(ctx: &AgentContext) -> &'static str {
    if ctx.failover.profile_count() == 1 {
        return "no failover attempted: the set has a single profile";
    }
    if ctx.failover.profile_idx() + 1 >= ctx.failover.profile_count() {
        return "no failover attempted: the chain is already at its last profile";
    }
    "no failover attempted: every endpoint ahead is marked in the shared fact table or shares the blocked account"
}

/// The operator-facing message for a stop the shared table caused: which endpoint
/// and profile stopped serving, why the chain did not take over, when the
/// upstream said service resumes, and the evidence the fact carries.
///
/// Shaped like `stop_message`, whose evidence is an error: here no request was
/// made, so the fact itself is the evidence, and the resumption sentence is the
/// one `fact_error` carries: the two diagnostic faces agree about one fact.
fn fact_stop_message(
    stop_reason: &'static str,
    ctx: &AgentContext,
    reading: &TableReading,
    chain_note: &str,
) -> String {
    let profile = ctx.failover.current_profile();
    // An account-level fact parks with the payload instead, so what reaches here
    // is an endpoint-level fact — and the write side records the upstream's reset
    // hint for every kind it writes, so this one has a moment to report whenever
    // the upstream named one. The sentence is the one fact_error carries: one
    // fact, one wording, on both diagnostic faces.
    let Some(fact) = reading.blocked.as_ref() else {
        return format!(
            "{}: endpoint {} (profile {}); {}; evidence: the shared table has nothing to say",
            stop_reason, profile.endpoint, profile.id, chain_note
        );
    };
    format!(
        "{}: endpoint {} (profile {}); {}; {}; evidence: {}",
        stop_reason,
        profile.endpoint,
        profile.id,
        chain_note,
        resumption_note(fact.block.until),
        fact.block.detail
    )
}

/// The observation line a skipped send leaves: the fact hit rate is counted from
/// these lines (one per skipped send), so it names the agent, the endpoint it
/// would have used, the account hint, the moment the upstream said the endpoint
/// reopens, and why the send did not happen.
fn log_fact_skip(ctx: &AgentContext, endpoint_id: &str, reading: &TableReading, reason: &str) {
    let Some(fact) = reading.blocked.as_ref() else {
        return;
    };
    info!(
        agent = ?ctx.config.agent_id,
        endpoint = endpoint_id,
        account_hint = %fact.account_hint,
        until = ?fact.block.until,
        skip_reason = reason,
        "shared fact skipped a send"
    );
}

/// Drive within-set failover for `trigger`: advance the chain, emit a `Failover` event on
/// advance, and map the [`FailoverOutcome`] to a [`FailoverStep`]. `reason` is the operator-facing
/// diagnostic captured from `trigger` before it is moved into [`advance_failover`].
///
/// `exhaustion` is the caller's reading of why the chain was walked: a transient
/// trigger leaves an exhausted chain to the outer re-run budget, while a class
/// another send cannot help with stops the round here.
async fn step_failover(
    ctx: &mut AgentContext,
    messages: &mut Vec<Message>,
    trigger: Error,
    reason: String,
    exhaustion: Exhaustion,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    round_cancel: &CancellationToken,
) -> FailoverStep {
    match advance_failover(ctx, std::mem::take(messages), trigger, round_cancel).await {
        FailoverOutcome::Advanced {
            from,
            to,
            messages: new_messages,
        } => {
            *messages = new_messages;
            // Under skip, `from`→`to` may jump over unbuildable intermediates; those are
            // warned inside advance_failover (not surfaced here).
            warn!(from = %from, to = %to, reason = %reason, "within-set failover");
            tx.send(AgentEvent::Failover { from, to, reason })
                .await
                .ok();
            // A different profile is a fresh execution: its send budget and its
            // deadline restart with it.
            ctx.start_episode();
            FailoverStep::Advanced
        }
        FailoverOutcome::ChainExhausted { reason, trigger } => {
            // An auth rejection at exhaustion drives the tagma's profile-source
            // refresh; both exits below keep reporting it. A fact-driven walk
            // brings the fact's own wording here, not a rejection, so its signal
            // comes from the arm that drove the walk.
            if let Some(payload) = rejection_payload(&trigger) {
                emit_gateway_signal(ctx, payload);
            }
            match exhaustion {
                // A class another send cannot help with: the chain is out of
                // candidates, and re-running would repeat the same failure.
                // The message names why the chain ran out, not just that it did.
                Exhaustion::ExternalOnly { stop_reason } => {
                    let note = reason.to_string();
                    return FailoverStep::Done(AcquireResult::Stop {
                        message: stop_message(stop_reason, ctx, trigger.as_ref(), &note),
                    });
                }
                // A quota or payment block: the round ends as a structured park
                // rather than as an error. Nothing is armed — the payload is the
                // whole reason, and recovery comes from an external trigger.
                Exhaustion::QuotaBlocked(info) => {
                    return FailoverStep::Done(AcquireResult::Outcome(AgentOutcome::QuotaBlocked(
                        info,
                    )));
                }
                // Transient: fall through to the round-end the re-run budget owns.
                Exhaustion::Transient => {}
            }
            // Chain exhaustion is a defined round-end (sibling of MaxRoundsExceeded),
            // surfaced as a distinguishable terminal outcome rather than a generic
            // `Err`. `trigger` is an `anyhow::Error`; `render_error` de-duplicates its
            // source chain and surfaces any HTTP error body (`as_ref` is required —
            // `anyhow::Error` does not impl `std::error::Error`).
            FailoverStep::Done(AcquireResult::Outcome(
                AgentOutcome::FailoverChainExhausted {
                    reason,
                    detail: crate::llm_error::render_error(trigger.as_ref()),
                },
            ))
        }
        FailoverOutcome::Cancelled => {
            FailoverStep::Done(AcquireResult::Outcome(AgentOutcome::Cancelled))
        }
        FailoverOutcome::BudgetExceeded { consumed, budget } => {
            FailoverStep::Done(AcquireResult::Outcome(AgentOutcome::TokenBudgetExceeded {
                consumed,
                budget,
            }))
        }
    }
}

/// Advance the within-set failover chain on a terminal endpoint failure (`trigger`).
///
/// Walks the chain forward from the active profile and lands on the first candidate that is
/// **both buildable and window-feasible**. **Skip:** a candidate whose
/// `build_client` fails, *or* whose declared `max_context_window` violates the configured budget
/// shape (`AgentConfig::try_context_window`), is `warn!`-ed and skipped. The window check runs
/// **before** `advance_to` (which is forward-only and cannot roll back), so the agent is never
/// left with the client swapped to a model whose window can't serve the budget. On a successful
/// build + feasible window it commits the advance, swaps the client, re-applies the new profile's
/// context window, re-syncs the pinned-budget guard, and compacts the carried context if it now
/// exceeds the (possibly smaller) window. The (possibly recomputed) `messages` return via
/// [`FailoverOutcome::Advanced`] so the caller rebinds its round-local.
///
/// This is the sole driver of `FailoverState` advancement; the round loop just dispatches on the
/// outcome. The `AgentEvent::Failover` emission and the retry-record flush stay in the caller
/// (the arm) — keeping this function free of channel side effects so it is unit-testable without
/// an mpsc sender. `summarize_and_evict` has no internal cancel select, so a cancel fired *during*
/// compaction completes it and returns `Advanced` (the cancel is observed on the next loop
/// iteration); this is pre-existing behavior, inherited from the inline arm.
pub(crate) async fn advance_failover(
    ctx: &mut AgentContext,
    prior_messages: Vec<Message>,
    trigger: anyhow::Error,
    round_cancel: &CancellationToken,
) -> FailoverOutcome {
    // Honor a cancel that raced in between the failover decision and the chain advance — takes
    // precedence over chain-exhaustion.
    if round_cancel.is_cancelled() {
        return FailoverOutcome::Cancelled;
    }
    // No candidate ahead — single-profile set, or already at the chain tail. Distinguish the
    // two: a single-profile set means failover was never configured, while a multi-profile
    // set at its tail means the chain was advanced through and now the last profile failed.
    if !ctx.failover.can_advance() {
        let reason = if ctx.failover.profile_count() == 1 {
            FailoverChainExhaustion::NoFailoverConfigured
        } else {
            FailoverChainExhaustion::AllBackupsExhausted
        };
        return FailoverOutcome::ChainExhausted { reason, trigger };
    }
    let from = ctx.failover.current_profile().id.clone();
    let mut offset = 1usize;
    // Track why candidates were skipped so the terminal exhaustion reason steers the operator:
    // a window-infeasible chain means the budget shape needs tuning; an unbuildable chain means
    // credentials/endpoint config; a chain the shared table has marked means another account or
    // another endpoint has to be configured. (See the coalescing rule on
    // `FailoverChainExhaustion`.)
    let mut skipped_infeasible = false;
    let mut skipped_unbuildable = false;
    let mut skipped_marked = false;
    // The shared table's own reading is what keeps the walk off a marked
    // endpoint; the fact being escaped was written before the walk starts, so an
    // account-level one already answers for the whole account.
    let now = timefmt::now_epoch();
    while let Some(candidate) = ctx.failover.candidate_profile(offset) {
        // A candidate the table has already marked cannot serve: switching into it
        // would burn an attempt to learn what another agent already paid for.
        if let Some(health) = ctx.endpoint_health.as_deref()
            && candidate_marked(health, &candidate.endpoint, now)
        {
            warn!(
                from = %from,
                candidate = %candidate.id,
                "failover candidate marked in the shared fact table, skipping"
            );
            skipped_marked = true;
            offset += 1;
            continue;
        }
        match ctx.failover.build_client(&candidate) {
            Ok(new_client) => {
                // The candidate builds — but its declared window must also fit the budget shape.
                // Pre-check BEFORE committing (advance_to is forward-only, no rollback): an
                // infeasible window is skipped like an unbuildable backend, so the agent never
                // ends up with the client swapped to a model whose window can't serve the budget.
                if let Err(err) = ctx.config.try_context_window(candidate.max_context_window) {
                    warn!(
                        from = %from,
                        candidate = %candidate.id,
                        window = candidate.max_context_window,
                        "failover candidate window infeasible for budget shape, skipping: {err:#}"
                    );
                    skipped_infeasible = true;
                    offset += 1;
                    continue;
                }
                // Commit only after a successful build + feasible window: the index advances
                // and the client swaps once we know the new profile is usable.
                let target_idx = ctx.failover.profile_idx() + offset;
                ctx.failover.advance_to(target_idx);
                ctx.client = new_client;
                // Fresh chain for the fresh client: response ids issued by the old
                // provider are meaningless to the new, so the next request resends fully.
                // The fresh chain's store flag rides the profile we just advanced to
                // (`None` = client default, store on).
                ctx.conversation = ctx
                    .client
                    .conversation()
                    .with_store(candidate.store.unwrap_or(true));
                reapply_window(ctx, &from).await;
                // The carried context may now exceed the (possibly smaller) window — compact so
                // the rebuilt request fits. summarize_and_evict no-ops when the context already
                // fits (it checks before any LLM call); it uses ctx.client, already swapped to
                // the working profile above.
                let messages = match summarize_and_evict(ctx).await {
                    Ok(CompactOutcome::Compacted) => compose_context(ctx.store.clone()).await,
                    Ok(CompactOutcome::NothingToCompact) => prior_messages,
                    Ok(CompactOutcome::BudgetExceeded { consumed, budget }) => {
                        return FailoverOutcome::BudgetExceeded { consumed, budget };
                    }
                    Err(err) => {
                        warn!("failover compaction failed, sending as-is: {err:#}");
                        prior_messages
                    }
                };
                return FailoverOutcome::Advanced {
                    from,
                    to: candidate.id.clone(),
                    messages,
                };
            }
            Err(err) => {
                warn!(
                    from = %from,
                    candidate = %candidate.id,
                    "failover candidate backend unbuildable, skipping: {err:#}"
                );
                skipped_unbuildable = true;
                offset += 1;
            }
        }
    }
    // Every remaining candidate was skipped (each warned above). Surface the original trigger —
    // it is why failover was attempted, and the actionable cause for the operator. Prefer the
    // infeasible reason when present (the subtler, more actionable mode); per-candidate warns
    // carry each skip's precise cause.
    let reason = if skipped_infeasible {
        FailoverChainExhaustion::AllCandidatesInfeasible
    } else if skipped_unbuildable {
        FailoverChainExhaustion::AllCandidatesUnbuildable
    } else if skipped_marked {
        // Every candidate ahead is marked in the shared table: the chain has no
        // endpoint left that could serve this request, which is what "all backups
        // exhausted" says. The per-candidate warns name the marking itself.
        FailoverChainExhaustion::AllBackupsExhausted
    } else {
        FailoverChainExhaustion::AllCandidatesUnbuildable
    };
    FailoverOutcome::ChainExhausted { reason, trigger }
}

/// Re-apply the active profile's declared context window to the config and re-sync the store's
/// pinned-budget guard. Called after a failover advance swaps to a profile that may declare a
/// different window (within-set heterogeneous windows are supported).
///
/// The window was already pre-checked feasible in `advance_failover` (before the commit), so
/// `set_context_window` is expected to succeed here. The `warn!`-and-keep-prior branch stays as
/// defense-in-depth: if an invariant somehow still fails post-commit, keeping the prior window is
/// safer than `?`-propagating into a half-applied state (index advanced, window stale). The
/// pinned-budget re-sync + `mark_needs_full_estimate` are unconditional and idempotent.
async fn reapply_window(ctx: &mut AgentContext, from: &str) {
    let new_window = ctx.failover.current_profile().max_context_window;
    if let Err(err) = ctx.config.set_context_window(new_window) {
        warn!(
            from = %from,
            target_window = new_window,
            "failed to re-apply context window on failover, keeping prior: {err:#}"
        );
    }
    // Failover swapped the active profile: the new provider's tokenizer renders the same prompt
    // to a different count, so the persisted `last_prompt_tokens` anchor is invalid — force a
    // full estimate on the next gate until a response re-anchors.
    let mut store = ctx.store.lock().await;
    store.set_pinned_budget(ctx.config.pinned_budget());
    store.mark_needs_full_estimate();
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::retry::AttemptOrigin;
    use crate::retry::{Episode, RetryCall, RetryPolicy, stream_with_retry};
    use crate::test_support::{
        MapSource, RecordingBackend, RecordingHealth, account_hint, ctx_from_source, profile,
        request, system_msg, user_msg,
    };
    use just_llm_client::LlmBackend;
    use just_llm_client::types::generation::ReasoningEffort;
    use kallipai_common::facts::{Block, BlockKind, QUOTA_RESET_TRUST_MAX_SECS};
    use kallipai_common::retry::RetryKind;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Effort rides the current profile into the wire request: a profile declaring
    /// `effort` produces requests carrying that `reasoning_effort`.
    #[tokio::test]
    async fn acquire_stream_injects_profile_effort_into_the_request() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut p = profile("a", "p1", 100_000);
        p.effort = Some(ReasoningEffort::High);
        let mut ctx = ctx_from_source(vec![p], source, RetryPolicy::default()).await;

        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &CancellationToken::new(),
            0,
        )
        .await;
        assert!(matches!(outcome, AcquireResult::Consumed(_)));
        let reqs = backend.take_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].reasoning_effort, Some(ReasoningEffort::High));
    }

    /// Store rides the profile into the rebuilt conversation: advancing to a candidate
    /// declaring `store = false` yields a stateless chain even though the backend supports
    /// continuation, while `None` keeps the client default (stateful).
    #[tokio::test]
    async fn failover_advance_applies_candidate_store_to_the_conversation() {
        let backend = RecordingBackend::new();
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p3".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        let mut stateless = profile("b", "p2", 100_000);
        stateless.store = Some(false);
        let mut ctx = ctx_from_source(
            vec![
                profile("a", "p1", 100_000),
                stateless,
                profile("c", "p3", 100_000),
            ],
            source,
            RetryPolicy::default(),
        )
        .await;
        assert!(
            ctx.conversation.is_stateful(),
            "the opening chain keeps the client default"
        );

        let outcome = advance_failover(
            &mut ctx,
            vec![user_msg("q1")],
            anyhow::anyhow!("endpoint dead"),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(outcome, FailoverOutcome::Advanced { .. }));
        assert!(
            !ctx.conversation.is_stateful(),
            "store = false advances to a stateless chain"
        );

        let outcome = advance_failover(
            &mut ctx,
            vec![user_msg("q2")],
            anyhow::anyhow!("endpoint dead"),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(outcome, FailoverOutcome::Advanced { .. }));
        assert!(
            ctx.conversation.is_stateful(),
            "store = None advances back to the client default"
        );
    }

    /// Failover replaces the client AND the conversation chain: after an advance, the
    /// next request opens full (no `previous_response_id`), because response ids issued
    /// by the previous provider are meaningless to the new one.
    #[tokio::test]
    async fn failover_advance_rebuilds_the_conversation_chain() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        backend.queue_stream("resp-2");
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;

        // One successful turn on the opening profile leaves a capture in the chain.
        let mut stream = stream_with_retry(
            RetryCall {
                conversation: &mut ctx.conversation,
                request: request("a-model", vec![user_msg("q1")]),
                policy: &ctx.config.retry_policy,
                round: 0,
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "p1",
                health: None,
            },
            &tokio::sync::mpsc::channel(16).0,
            &mut Vec::new(),
            CancellationToken::new(),
        )
        .await
        .expect("turn streams");
        while let Some(event) = stream.next().await {
            event.expect("turn events are healthy");
        }
        let response = stream
            .into_response()
            .expect("a drained test stream reaches `End` with a response id");
        ctx.conversation
            .adopt(response)
            .expect("the just-prepared turn adopts cleanly");
        assert!(ctx.conversation.last_message().is_some(), "turn 1 anchored");

        // Advance the chain the way a Failover outcome does.
        let outcome = advance_failover(
            &mut ctx,
            vec![user_msg("q1")],
            anyhow::anyhow!("endpoint dead"),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(outcome, FailoverOutcome::Advanced { .. }));
        assert_eq!(ctx.failover.profile_idx(), 1);
        // Fresh chain: the previous provider's mirror is gone.
        assert!(ctx.conversation.last_message().is_none());

        // The next turn on the new profile opens full — no continuation id.
        let mut stream = stream_with_retry(
            RetryCall {
                conversation: &mut ctx.conversation,
                request: request("b-model", vec![user_msg("q1")]),
                policy: &ctx.config.retry_policy,
                round: 0,
                episode: &mut Episode::new(timefmt::now_epoch()),
                endpoint_id: "p2",
                health: None,
            },
            &tokio::sync::mpsc::channel(16).0,
            &mut Vec::new(),
            CancellationToken::new(),
        )
        .await
        .expect("post-failover turn streams");
        while let Some(event) = stream.next().await {
            event.expect("turn events are healthy");
        }
        let response = stream
            .into_response()
            .expect("a drained test stream reaches `End` with a response id");
        ctx.conversation
            .adopt(response)
            .expect("the just-prepared turn adopts cleanly");

        let requests = backend.take_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].previous_response_id, None, "turn 1 opens full");
        assert_eq!(
            requests[1].previous_response_id, None,
            "a rebuilt chain cannot continue the old provider's ids"
        );
        assert_eq!(requests[1].messages, vec![user_msg("q1")]);
    }

    // -- deterministic image-rejection degrade -------------------------

    use crate::context::compose_context;
    use crate::history::HistoryWriter;
    use just_llm_client::TransportError;
    use just_llm_client::types::generation::ContentPart;

    fn fatal_400() -> Error {
        BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::BAD_REQUEST,
                body: "images are not supported".to_owned(),
            },
        )
        .into()
    }

    /// A ctx whose single profile sits on a `RecordingBackend`, with a live
    /// image turn (turn 0) plus its matching history sidecar record.
    async fn image_ctx(
        backend: Arc<RecordingBackend>,
    ) -> (AgentContext, tempfile::TempDir, uuid::Uuid) {
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            Arc::clone(&backend) as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("p1", "p1", 100_000)],
            source,
            crate::retry::RetryPolicy::default(),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        ctx.agent_dir = Some(dir.path().to_owned());
        ctx.history = Some(HistoryWriter::new(dir.path().to_owned()));

        let record_id = uuid::Uuid::from_u128(0xD00D);
        let (turn_id, estimated) = {
            let mut guard = ctx.store.lock().await;
            guard.push_turn(vec![Message::user_parts(vec![
                ContentPart::Text {
                    text: "chart caption".to_owned(),
                },
                ContentPart::Image {
                    source: just_llm_client::types::generation::ImageSource::Base64 {
                        data: "aGk=".to_owned(),
                        media_type: "image/png".to_owned(),
                    },
                    detail: None,
                },
            ])])
        };
        ctx.append_history(
            Some(turn_id.0),
            &[Message::user("chart caption")],
            estimated,
            crate::history::RecordKind::Turn,
            None,
            &[crate::history::AttachmentRef {
                modality: kallipai_common::protocol::Modality::Image,
                record_id: Some(record_id),
                media_type: "image/png".to_owned(),
                blob_id: format!("sha256-{record_id:064x}"),
                caption: Some("chart caption".to_owned()),
            }],
        );
        (ctx, dir, record_id)
    }

    #[tokio::test]
    async fn degrade_strips_retries_and_records_the_invalidation() {
        let backend = RecordingBackend::new();
        // Attempt 1: the provider 400s the request; attempt 2 (stripped) streams.
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::BAD_REQUEST,
                body: "unsupported image".to_owned(),
            },
        ));
        backend.queue_stream("resp-1");
        let (mut ctx, _dir, _record_id) = image_ctx(backend.clone()).await;

        let messages = compose_context(ctx.store.clone()).await;
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        // The round's first acquire fails 400 on the image-carrying request.
        let error = match acquire_stream(
            &mut ctx,
            messages,
            Vec::new(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
            0,
        )
        .await
        {
            AcquireResult::Error(e) => e,
            _other => panic!("expected a fatal image rejection"),
        };
        let result = degrade_on_image_rejection(
            &mut ctx,
            &error,
            Vec::new(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
            0,
        )
        .await;
        assert!(matches!(result, Some(AcquireResult::Consumed(_))));

        // The live image turn is text-only now, plus the plain-language note.
        {
            let guard = ctx.store.lock().await;
            let image_turn = &guard.turns()[0];
            assert!(
                image_turn
                    .messages
                    .iter()
                    .all(|m| !crate::context::message_has_images(m))
            );
            let note = guard.turns().back().unwrap();
            assert!(format!("{:?}", note.messages).contains("[system]"));
        }
    }

    #[tokio::test]
    async fn degraded_retry_failure_leaves_the_store_untouched() {
        let backend = RecordingBackend::new();
        // Both attempts 400: the images were not the (only) problem.
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::BAD_REQUEST,
                body: "still bad".to_owned(),
            },
        ));
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::BAD_REQUEST,
                body: "still bad".to_owned(),
            },
        ));
        let (mut ctx, _dir, record_id) = image_ctx(backend.clone()).await;

        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let result = degrade_on_image_rejection(
            &mut ctx,
            &fatal_400(),
            Vec::new(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
            0,
        )
        .await;
        assert!(result.is_none());

        // Nothing moved: the image turn keeps its parts, no note turn was
        // appended.
        {
            let guard = ctx.store.lock().await;
            assert_eq!(guard.turn_count(), 1);
            assert!(
                guard.turns()[0]
                    .messages
                    .iter()
                    .any(crate::context::message_has_images)
            );
        }
        let _ = record_id;
    }

    #[test]
    fn fatal_provider_status_filters_non_request_level_statuses() {
        let mk = |status: reqwest::StatusCode| {
            BackendError::provider(
                "recording",
                TransportError::HttpStatus {
                    status,
                    body: String::new(),
                },
            )
            .into()
        };
        assert_eq!(
            fatal_provider_status(&mk(reqwest::StatusCode::BAD_REQUEST)),
            Some(reqwest::StatusCode::BAD_REQUEST)
        );
        // Auth/404/5xx statuses belong to failover/retry arms — never degrade.
        assert_eq!(
            fatal_provider_status(&mk(reqwest::StatusCode::UNAUTHORIZED)),
            None
        );
        assert_eq!(
            fatal_provider_status(&mk(reqwest::StatusCode::NOT_FOUND)),
            None
        );
        assert_eq!(
            fatal_provider_status(&mk(reqwest::StatusCode::INTERNAL_SERVER_ERROR)),
            None
        );
        // A pre-flight (non-provider) failure has no status at all.
        assert_eq!(fatal_provider_status(&anyhow::anyhow!("boom")), None);
    }

    /// End to end: a mid-stream drop keeps the anchor, so the in-place retry re-sends
    /// the same delta, and the next turn continues from the retry's response — a failed
    /// turn never pollutes the chain.
    #[tokio::test]
    async fn mid_stream_drop_retries_delta_and_the_anchor_advances_past_it() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        backend.queue_dropped_stream(
            vec![just_llm_client::types::generation::GenerationEvent::Text {
                delta: "partial".to_owned(),
            }],
            just_llm_client::TransportError::InvalidResponse(
                "stream closed before the terminal event".to_owned(),
            ),
        );
        backend.queue_stream("resp-2");
        backend.queue_stream("resp-3");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let policy = RetryPolicy {
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
            ..RetryPolicy::default()
        };
        let mut ctx = ctx_from_source(vec![profile("a", "p1", 100_000)], source, policy).await;

        // Each turn below is a round: production reaches `acquire_stream` through the
        // episode boundary that restarts the retry ledger, so the drop in turn 2 is that
        // episode's first send.
        ctx.start_episode();
        // Turn 1: full open; resp-1 settles the anchor.
        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &tokio_util::sync::CancellationToken::new(),
            0,
        )
        .await;
        assert!(matches!(outcome, AcquireResult::Consumed(_)));
        let mirror1 = ctx.conversation.last_message().expect("turn 1 anchored");

        // Turn 2: the first stream drops after one delta; the in-place retry succeeds.
        ctx.start_episode();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1"), mirror1.clone(), user_msg("q2")],
            Vec::new(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
            1,
        )
        .await;
        assert!(matches!(outcome, AcquireResult::Consumed(_)));

        // The dropped attempt told downstream to fold the partial it had rendered.
        let mut resets = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::event::AgentEvent::StreamReset {
                attempt,
                max_attempts,
                ..
            } = event
            {
                resets.push((attempt, max_attempts));
            }
        }
        assert_eq!(resets, vec![(1, ctx.config.retry_policy.max_retries)]);

        // Turn 3: continues from the retry's response — the drop left no trace.
        ctx.start_episode();
        let mirror2 = ctx.conversation.last_message().expect("turn 2 anchored");
        let outcome = acquire_stream(
            &mut ctx,
            vec![
                user_msg("q1"),
                mirror1.clone(),
                user_msg("q2"),
                mirror2,
                user_msg("q3"),
            ],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &tokio_util::sync::CancellationToken::new(),
            2,
        )
        .await;
        assert!(matches!(outcome, AcquireResult::Consumed(_)));

        let requests = backend.take_requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].previous_response_id, None, "turn 1 opens full");
        assert_eq!(
            requests[0].messages,
            vec![system_msg("sys"), user_msg("q1")],
            "full context, system prompt riding the opening turn"
        );
        assert_eq!(
            requests[1].previous_response_id.as_deref(),
            Some("resp-1"),
            "turn 2 chains the settled turn"
        );
        assert_eq!(requests[1].messages, vec![user_msg("q2")], "delta only");
        assert_eq!(
            requests[2].previous_response_id.as_deref(),
            Some("resp-1"),
            "the drop kept the anchor: the retry re-sends the same delta"
        );
        assert_eq!(requests[2].messages, vec![user_msg("q2")]);
        assert_eq!(
            requests[3].previous_response_id.as_deref(),
            Some("resp-2"),
            "the anchor advanced with the retry's response"
        );
        assert_eq!(requests[3].messages, vec![user_msg("q3")], "delta only");
    }

    /// The episode deadline bounds a mid-stream retry loop: a drop arriving with
    /// `retry_timeout` already spent ends the round instead of re-sending, however
    /// much send budget is left. The drop is the episode's first send, so the exit
    /// comes from the episode's own deadline; the persisted retry log plays no part.
    /// The same drop inside the window retries, and a repeated shape stops the round
    /// outright (see the shape-repeat tests).
    #[tokio::test]
    async fn a_mid_stream_drop_past_the_deadline_advances_the_chain() {
        let backend = RecordingBackend::new();
        backend.queue_dropped_stream(
            vec![just_llm_client::types::generation::GenerationEvent::Text {
                delta: "partial".to_owned(),
            }],
            just_llm_client::TransportError::InvalidResponse(
                "stream closed before the terminal event".to_owned(),
            ),
        );
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let policy = RetryPolicy {
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
            ..RetryPolicy::default()
        };
        let mut ctx = ctx_from_source(vec![profile("a", "p1", 100_000)], source, policy).await;
        // The episode started well before its own deadline window.
        ctx.episode = Episode::new(timefmt::now_epoch() - 600);

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
            0,
        )
        .await;

        // One send only, and a single-profile set has nowhere to fail over.
        match outcome {
            AcquireResult::Outcome(AgentOutcome::FailoverChainExhausted { .. }) => {}
            _ => panic!("expected FailoverChainExhausted, got a different outcome"),
        }
        assert_eq!(ctx.episode.attempts(), 1, "the drop spent one send");
        assert_eq!(backend.take_requests().len(), 1, "the drop was not re-sent");
        let mut resets = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let crate::event::AgentEvent::StreamReset {
                attempt,
                delay_secs,
                ..
            } = event
            {
                resets.push((attempt, delay_secs));
            }
        }
        assert_eq!(
            resets,
            vec![(1, 0.0)],
            "the exhausted arm voids the partial with no wait"
        );
    }

    /// A cancel while the stream is in flight adopts nothing — the anchor stays on
    /// the last settled turn and the next turn continues it with a delta. (The cancel
    /// path emits no `StreamReset`; that asymmetry is established behavior.)
    #[tokio::test]
    async fn mid_stream_cancel_keeps_the_anchor_and_the_next_turn_continues_it() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        backend.queue_pending_stream();
        backend.queue_stream("resp-2");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;

        // Turn 1: full open; resp-1 settles the anchor.
        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &tokio_util::sync::CancellationToken::new(),
            0,
        )
        .await;
        assert!(matches!(outcome, AcquireResult::Consumed(_)));
        let mirror1 = ctx.conversation.last_message().expect("turn 1 anchored");

        // Turn 2: the stream never yields; the round cancel fires mid-flight.
        let cancel = tokio_util::sync::CancellationToken::new();
        let canceller = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                cancel.cancel();
            })
        };
        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1"), mirror1.clone(), user_msg("q2")],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &cancel,
            1,
        )
        .await;
        canceller.await.unwrap();
        assert!(
            matches!(
                outcome,
                AcquireResult::Outcome(crate::event::AgentOutcome::Cancelled)
            ),
            "the round cancels mid-stream"
        );
        assert_eq!(
            ctx.conversation.last_message().as_ref(),
            Some(&mirror1),
            "nothing was adopted past the settled turn"
        );

        // Turn 3: continues the untouched anchor with a delta.
        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1"), mirror1, user_msg("q3")],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &tokio_util::sync::CancellationToken::new(),
            2,
        )
        .await;
        assert!(matches!(outcome, AcquireResult::Consumed(_)));

        let requests = backend.take_requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].previous_response_id, None, "turn 1 opens full");
        assert_eq!(
            requests[1].previous_response_id.as_deref(),
            Some("resp-1"),
            "the cancelled turn chained normally"
        );
        assert_eq!(requests[1].messages, vec![user_msg("q2")], "delta only");
        assert_eq!(
            requests[2].previous_response_id.as_deref(),
            Some("resp-1"),
            "the cancel kept the anchor: the next turn continues it"
        );
        assert_eq!(requests[2].messages, vec![user_msg("q3")], "delta only");
    }

    // -- gateway auth-signal emission ------------------------------------

    #[tokio::test]
    async fn gateway_signal_carries_status_and_code_on_exhaustion_trigger() {
        let mut ctx = ctx_from_source(
            vec![profile("p1", "p1", 100_000)],
            Arc::new(MapSource(HashMap::from([(
                "p1".to_string(),
                RecordingBackend::new() as Arc<dyn LlmBackend>,
            )]))),
            RetryPolicy::default(),
        )
        .await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.gateway_signals = Some(tx);
        let trigger: Error = BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::UNAUTHORIZED,
                body: r#"{"error":{"message":"revoked","code":"key_revoked"}}"#.to_owned(),
            },
        )
        .into();
        emit_gateway_signal(&ctx, rejection_payload(&trigger).unwrap());
        let signal = rx.recv().await.expect("exactly one signal");
        assert_eq!(signal.status, 401);
        assert_eq!(signal.code.as_deref(), Some("key_revoked"));
        assert!(rx.try_recv().is_err(), "no second signal");
    }

    #[tokio::test]
    async fn gateway_signal_is_silent_without_a_channel() {
        let ctx = ctx_from_source(
            vec![profile("p1", "p1", 100_000)],
            Arc::new(MapSource(HashMap::from([(
                "p1".to_string(),
                RecordingBackend::new() as Arc<dyn LlmBackend>,
            )]))),
            RetryPolicy::default(),
        )
        .await;
        let trigger: Error = BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::FORBIDDEN,
                body: "denied".to_owned(),
            },
        )
        .into();
        emit_gateway_signal(&ctx, rejection_payload(&trigger).unwrap());
    }

    #[tokio::test]
    async fn gateway_signal_ignores_non_auth_statuses() {
        let mut ctx = ctx_from_source(
            vec![profile("p1", "p1", 100_000)],
            Arc::new(MapSource(HashMap::from([(
                "p1".to_string(),
                RecordingBackend::new() as Arc<dyn LlmBackend>,
            )]))),
            RetryPolicy::default(),
        )
        .await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.gateway_signals = Some(tx);
        let trigger: Error = BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::BAD_REQUEST,
                body: "bad".to_owned(),
            },
        )
        .into();
        emit_gateway_signal(&ctx, rejection_payload(&trigger).unwrap());
        assert!(rx.try_recv().is_err(), "400 never signals");
    }

    /// A spent episode advances the chain instead of sending: the budget is the
    /// ledger, so the loop turns a spent one into a failover, not a round error.
    #[tokio::test]
    async fn a_spent_episode_advances_the_chain_without_sending() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            RetryPolicy {
                max_retries: 1,
                ..RetryPolicy::default()
            },
        )
        .await;

        // Spend the opening episode's two sends before the loop's first pass.
        ctx.start_episode();
        ctx.episode = ctx.episode.after_attempt();
        ctx.episode = ctx.episode.after_attempt();

        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &CancellationToken::new(),
            0,
        )
        .await;

        assert!(matches!(outcome, AcquireResult::Consumed(_)));
        let reqs = backend.take_requests();
        assert_eq!(reqs.len(), 1, "only the second profile is asked");
        assert_eq!(
            reqs[0].model, "b-model",
            "the opening profile's episode is spent"
        );
    }

    /// The persisted retry log is an observation surface, not a budget: in-window
    /// records for the endpoint leave the episode's sends untouched.
    #[tokio::test]
    async fn in_window_log_records_do_not_shrink_the_episode_budget() {
        use just_llm_client::{BackendError, TransportError};

        let backend = RecordingBackend::new();
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy {
                max_retries: 2,
                base_delay: std::time::Duration::from_millis(1),
                max_delay: std::time::Duration::from_millis(2),
                retry_timeout: std::time::Duration::from_secs(10),
            },
        )
        .await;

        // Three records for this endpoint are already in the log: the episode's own
        // sends are unaffected by what the log holds.
        let now = timefmt::now_epoch();
        let records = (0..3)
            .map(|i| RetryRecord {
                timestamp: now,
                round: 0,
                attempt: i + 1,
                max_attempts: 2,
                error: "endpoint down".to_owned(),
                delay_secs: 0.0,
                endpoint: Some("p1".to_owned()),
                kind: RetryKind::Server,
                quota_reset: None,
            })
            .collect::<Vec<_>>();
        {
            let mut store = ctx.store.lock().await;
            store.record_retries(records, retry_keep_since(&ctx));
        }

        for _ in 0..3 {
            backend.queue_error(BackendError::provider(
                "recording",
                TransportError::HttpStatus {
                    status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    body: "down".to_owned(),
                },
            ));
        }

        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &CancellationToken::new(),
            0,
        )
        .await;

        assert!(matches!(
            outcome,
            AcquireResult::Outcome(AgentOutcome::FailoverChainExhausted { .. })
        ));
        assert_eq!(
            backend.take_requests().len(),
            3,
            "the episode spends max_retries + 1 sends whatever the log holds"
        );
    }

    /// A quota rejection is not retried at the same endpoint: the round advances to
    /// the next profile, which serves the turn.
    #[tokio::test]
    async fn a_quota_rejection_advances_to_the_next_profile() {
        let backend = RecordingBackend::new();
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                body: "slow down".to_owned(),
            },
        ));
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            RetryPolicy {
                max_retries: 2,
                ..RetryPolicy::default()
            },
        )
        .await;

        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &CancellationToken::new(),
            0,
        )
        .await;

        assert!(matches!(outcome, AcquireResult::Consumed(_)));
        let reqs = backend.take_requests();
        assert_eq!(reqs.len(), 2, "one send per profile, none repeated");
        assert_eq!(reqs[0].model, "a-model");
        assert_eq!(reqs[1].model, "b-model");
    }

    /// The same rejection with no candidate left parks the round with the
    /// structured reason: the endpoint, the account behind it, and the evidence.
    /// The endpoint is asked exactly once, and nothing is armed.
    #[tokio::test]
    async fn a_quota_rejection_with_no_candidate_left_parks() {
        let backend = RecordingBackend::new();
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                body: "slow down".to_owned(),
            },
        ));
        // A stream is queued so a second send would succeed: the request count pins
        // the claim that no second send happens.
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        ctx.endpoint_health = Some(health.clone());

        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &CancellationToken::new(),
            0,
        )
        .await;

        let AcquireResult::Outcome(AgentOutcome::QuotaBlocked(info)) = outcome else {
            panic!("a quota rejection with no candidate left parks with the structured reason");
        };
        assert_eq!(info.endpoint, "p1");
        assert!(
            !info.account_hint.is_empty(),
            "the fact the rejection wrote answers the read back: {info:?}"
        );
        assert_eq!(info.reset, None, "the body carried no resumption hint");
        assert!(info.detail.contains("429"), "{:?}", info.detail);
        assert_eq!(health.recorded().len(), 1, "one rejection, one fact");
        assert_eq!(
            backend.take_requests().len(),
            1,
            "the endpoint is asked exactly once"
        );
    }

    /// A candidate that exists but cannot be built is no candidate: the switch finds
    /// nothing to advance to, and the quota block parks the round.
    #[tokio::test]
    async fn an_unbuildable_candidate_turns_a_quota_switch_into_a_park() {
        let backend = RecordingBackend::new();
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                body: "slow down".to_owned(),
            },
        ));
        backend.queue_stream("resp-1");
        // p2 is declared but absent from the source, so building it fails.
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;

        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &CancellationToken::new(),
            0,
        )
        .await;

        assert!(
            matches!(
                outcome,
                AcquireResult::Outcome(AgentOutcome::QuotaBlocked(_))
            ),
            "an unbuildable candidate leaves nothing to switch to, so the block parks"
        );
        assert_eq!(ctx.failover.profile_idx(), 0, "the skip never adopts p2");
        assert_eq!(backend.take_requests().len(), 1);
    }

    /// An unbuildable candidate still reports the trigger to the gateway: the switch
    /// walks the chain, the walk finds nothing, and the exhaustion hook fires on the
    /// auth rejection that started it.
    #[tokio::test]
    async fn an_unbuildable_candidate_still_signals_the_gateway() {
        let backend = RecordingBackend::new();
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::UNAUTHORIZED,
                body: "revoked".to_owned(),
            },
        ));
        // p2 is declared but absent from the source, so building it fails.
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.gateway_signals = Some(signal_tx);
        let (tx, _rx) = tokio::sync::mpsc::channel(64);

        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tx,
            &CancellationToken::new(),
            0,
        )
        .await;

        let AcquireResult::Stop { message } = outcome else {
            panic!("an unbuildable candidate leaves nothing to switch to");
        };
        assert!(
            message.contains("endpoint or credentials rejected the request"),
            "{message}"
        );
        let signal = signal_rx
            .try_recv()
            .expect("the trigger reaches the gateway");
        assert_eq!(signal.status, 401);
        assert_eq!(backend.take_requests().len(), 1);
    }

    /// A single-profile set reports an auth rejection the same way: the chain has
    /// nowhere to go, so exhaustion is reached on the first candidate.
    #[tokio::test]
    async fn an_auth_rejection_signals_the_gateway_at_exhaustion() {
        let backend = RecordingBackend::new();
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::UNAUTHORIZED,
                body: "revoked".to_owned(),
            },
        ));
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("p1", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.gateway_signals = Some(signal_tx);
        let (tx, _rx) = tokio::sync::mpsc::channel(64);

        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tx,
            &CancellationToken::new(),
            0,
        )
        .await;

        let AcquireResult::Stop { message } = outcome else {
            panic!("a single profile has nothing to switch to");
        };
        assert!(
            message.contains("endpoint or credentials rejected the request"),
            "{message}"
        );
        let signal = signal_rx
            .try_recv()
            .expect("the trigger reaches the gateway");
        assert_eq!(signal.status, 401);
        assert_eq!(backend.take_requests().len(), 1);
    }

    /// The image degrade is a new execution: it strips the images and re-sends even
    /// when the failed attempt used the episode up (a zero-retry policy), because a
    /// spent ledger must not starve the one retry that can still succeed.
    #[tokio::test]
    async fn degrade_retries_when_the_episode_budget_is_spent() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let (mut ctx, _dir, _record_id) = image_ctx(backend.clone()).await;
        ctx.config.retry_policy.max_retries = 0;
        ctx.episode = ctx.episode.after_attempt();

        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let result = degrade_on_image_rejection(
            &mut ctx,
            &fatal_400(),
            Vec::new(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
            0,
        )
        .await;

        assert!(matches!(result, Some(AcquireResult::Consumed(_))));
        assert_eq!(
            backend.take_requests().len(),
            1,
            "the stripped request was sent despite the spent ledger"
        );
        assert_eq!(ctx.episode.attempts(), 1, "the retry ran its own episode");
    }

    /// A local client-configuration failure never reached a provider: the round stops
    /// on the first profile without walking the chain.
    #[tokio::test]
    async fn a_local_configuration_error_stops_without_walking_the_chain() {
        let backend = RecordingBackend::new();
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::InvalidConfig("no key"),
        ));
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;

        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tokio::sync::mpsc::channel(16).0,
            &CancellationToken::new(),
            0,
        )
        .await;

        let AcquireResult::Stop { message } = outcome else {
            panic!("a local configuration failure stops the round");
        };
        assert!(
            message.contains("local client configuration error"),
            "{message}"
        );
        assert!(message.contains("no failover attempted"), "{message}");
        assert_eq!(
            ctx.failover.profile_idx(),
            0,
            "a local failure is not worth another profile"
        );
        assert_eq!(backend.take_requests().len(), 1);
    }
    /// The mid-stream path writes what the rejection taught too: the 401 that
    /// stops this endpoint leaves an endpoint-level credential fact.
    #[tokio::test]
    async fn the_mid_stream_path_writes_the_fact_a_rejection_leaves() {
        let backend = RecordingBackend::new();
        backend.queue_dropped_stream(
            vec![just_llm_client::types::generation::GenerationEvent::Text {
                delta: "partial".to_owned(),
            }],
            just_llm_client::TransportError::HttpStatus {
                status: reqwest::StatusCode::UNAUTHORIZED,
                body: "{\"error\":\"bad key\"}".to_owned(),
            },
        );
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        ctx.endpoint_health = Some(health.clone());

        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let outcome = acquire_stream(
            &mut ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
            0,
        )
        .await;

        // A single-profile set has nowhere to fail over, so the round stops here
        // with the rejection in hand — and the fact is on the record either way.
        let AcquireResult::Stop { message } = outcome else {
            panic!("an endpoint-level rejection with no sibling to try stops the round");
        };
        assert!(message.contains("401"), "the stop carries it: {message}");
        let recorded = health.recorded();
        assert_eq!(recorded.len(), 1, "one rejection, one fact");
        assert_eq!(recorded[0].0, "p1");
        assert_eq!(recorded[0].1.kind, BlockKind::EndpointAuth);
        assert!(
            !recorded[0].1.kind.is_account_level(),
            "a credential rejection is about this endpoint, not the account"
        );
    }

    /// A fact another agent's failure left: the table's own value, seeded so a
    /// reader's verdict sees it without a tagma behind the context.
    fn seeded(kind: BlockKind, until: Option<u64>) -> Block {
        Block {
            kind,
            until,
            detail: "seeded evidence".to_owned(),
            status: None,
            recorded_at: timefmt::now_epoch(),
        }
    }

    /// The same fact with the status the upstream answered: what the signal face
    /// reads when a fact ends a round.
    fn seeded_with_status(kind: BlockKind, until: Option<u64>, status: u16) -> Block {
        Block {
            status: Some(status),
            ..seeded(kind, until)
        }
    }

    /// Drive one acquisition pass over `ctx`, with the retry records discarded.
    async fn acquire_once(ctx: &mut AgentContext) -> AcquireResult {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        acquire_stream(
            ctx,
            vec![user_msg("q1")],
            Vec::new(),
            &tx,
            &CancellationToken::new(),
            0,
        )
        .await
    }

    /// A fact the table carries stops a system-initiated attempt before it sends:
    /// the endpoint is asked nothing at all, and the account-level fact parks the
    /// round with the structured reason.
    #[tokio::test]
    async fn a_fact_the_table_carries_skips_the_send_and_parks() {
        let backend = RecordingBackend::new();
        // A stream is queued so a send would succeed: the request count pins the
        // claim that no send happens.
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded(BlockKind::Quota, None));
        ctx.endpoint_health = Some(health.clone());

        let outcome = acquire_once(&mut ctx).await;

        let AcquireResult::Outcome(AgentOutcome::QuotaBlocked(info)) = outcome else {
            panic!("an account-level fact with no candidate left parks the round");
        };
        assert_eq!(info.endpoint, "p1");
        assert!(
            !info.account_hint.is_empty(),
            "the seeded fact answers the read back: {info:?}"
        );
        assert!(info.detail.contains("seeded evidence"), "{:?}", info.detail);
        assert_eq!(
            backend.take_requests().len(),
            0,
            "the send the fact marks never happens"
        );
    }

    /// An endpoint-level fact accepts a sibling on the same account: the
    /// credentials are the endpoint's business, so another relay of the same
    /// credential is a candidate. Requiring a different account here would leave
    /// the sibling unused — the flip this case is written to catch.
    #[tokio::test]
    async fn an_endpoint_level_fact_advances_to_a_sibling_on_the_same_account() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded(BlockKind::EndpointAuth, None));
        health.share_account("p1", "p2");
        ctx.endpoint_health = Some(health.clone());

        let outcome = acquire_once(&mut ctx).await;

        assert!(
            matches!(outcome, AcquireResult::Consumed(_)),
            "the sibling serves the turn"
        );
        let requests = backend.take_requests();
        assert_eq!(requests.len(), 1, "only the sibling is asked");
        assert_eq!(
            requests[0].model, "b-model",
            "a same-account sibling is a candidate for an endpoint-level fact"
        );
    }

    /// An account-level fact does not accept one: the quota follows the
    /// credential, so a sibling on the same account would meet the same wall. The
    /// round parks instead, and neither endpoint is asked.
    #[tokio::test]
    async fn an_account_level_fact_refuses_a_sibling_on_the_same_account() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded(BlockKind::Quota, None));
        health.share_account("p1", "p2");
        ctx.endpoint_health = Some(health.clone());

        let outcome = acquire_once(&mut ctx).await;

        let AcquireResult::Outcome(AgentOutcome::QuotaBlocked(info)) = outcome else {
            panic!("a quota fact with only a same-account sibling parks the round");
        };
        assert_eq!(info.endpoint, "p1");
        assert_eq!(
            backend.take_requests().len(),
            0,
            "neither endpoint is asked"
        );
    }

    /// A marked candidate is skipped rather than chosen: the walk keeps looking,
    /// so a marked sibling does not hide a usable endpoint behind it.
    #[tokio::test]
    async fn a_marked_candidate_is_skipped_for_the_next_one() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p3".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        let mut ctx = ctx_from_source(
            vec![
                profile("a", "p1", 100_000),
                profile("b", "p2", 100_000),
                profile("c", "p3", 100_000),
            ],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded(BlockKind::Quota, None));
        // The sibling in the middle carries a fact of its own.
        health.seed("p2", seeded(BlockKind::EndpointAuth, None));
        ctx.endpoint_health = Some(health.clone());

        let outcome = acquire_once(&mut ctx).await;

        assert!(
            matches!(outcome, AcquireResult::Consumed(_)),
            "the unmarked sibling serves the turn"
        );
        let requests = backend.take_requests();
        assert_eq!(requests.len(), 1, "the marked sibling is never asked");
        assert_eq!(requests[0].model, "c-model");
    }

    /// The wake-triggered execution's first attempt sends once anyway: a user
    /// asking is served even against an endpoint the table has marked, at the
    /// price of one bounded attempt.
    #[tokio::test]
    async fn the_wake_first_attempt_sends_once_through_a_fact() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded(BlockKind::Quota, None));
        ctx.endpoint_health = Some(health.clone());
        ctx.attempt_origin = AttemptOrigin::External;

        let outcome = acquire_once(&mut ctx).await;

        assert!(
            matches!(outcome, AcquireResult::Consumed(_)),
            "the wake's own attempt is served"
        );
        assert_eq!(
            backend.take_requests().len(),
            1,
            "the user asking is served once, not skipped"
        );
    }

    /// A rejection that carries a resumption hint hands it to the payload: the
    /// park says when the upstream expects to serve again.
    #[tokio::test]
    async fn a_quota_rejection_with_a_reset_hint_carries_it() {
        let backend = RecordingBackend::new();
        backend.queue_error(BackendError::provider(
            "recording",
            TransportError::HttpStatus {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                body: "{\"error\":\"slow down\",\"retry_after\":120}".to_owned(),
            },
        ));
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        ctx.endpoint_health = Some(health.clone());
        let before = timefmt::now_epoch();

        let outcome = acquire_once(&mut ctx).await;

        let AcquireResult::Outcome(AgentOutcome::QuotaBlocked(info)) = outcome else {
            panic!("a quota rejection parks with the structured reason");
        };
        let reset = info.reset.expect("the upstream gave a moment to show");
        assert!(
            reset >= before + 120,
            "the hint is the upstream's own value: {reset} vs {before}"
        );
        assert!(!info.account_hint.is_empty(), "{info:?}");
    }

    /// A fact stop on a set with nothing to walk says so, rather than claiming
    /// every endpoint ahead is marked.
    #[tokio::test]
    async fn a_fact_stop_on_a_single_profile_names_that() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded(BlockKind::EndpointAuth, None));
        ctx.endpoint_health = Some(health.clone());

        let outcome = acquire_once(&mut ctx).await;

        let AcquireResult::Stop { message } = outcome else {
            panic!("an endpoint-level fact with no sibling stops the round");
        };
        assert!(
            message.contains("the set has a single profile"),
            "{message}"
        );
    }

    /// An endpoint-level fact that carries a reset hint reports it: the stop
    /// message names the moment the upstream gave, in the same sentence the
    /// failover log line uses.
    #[tokio::test]
    async fn a_fact_stop_reports_the_resumption_the_upstream_named() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        let until = timefmt::now_epoch() + 300;
        health.seed("p1", seeded(BlockKind::EndpointAuth, Some(until)));
        ctx.endpoint_health = Some(health.clone());

        let outcome = acquire_once(&mut ctx).await;

        let AcquireResult::Stop { message } = outcome else {
            panic!("an endpoint-level fact with no sibling stops the round");
        };
        assert!(
            message.contains(&format!(
                "upstream said it reopens at {until} (unix seconds)"
            )),
            "{message}"
        );
        assert_eq!(backend.take_requests().len(), 0, "nothing is asked");
    }

    /// The diagnostic faces quote the upstream's own value: a hint past the trust
    /// bound still reads as the upstream gave it, while the display face of the
    /// same fact reads as unknown — the bound is applied once, where a value
    /// becomes a promise a reader may show.
    #[tokio::test]
    async fn a_fact_stop_quotes_the_hint_the_display_face_clamps() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        let recorded_at = timefmt::now_epoch();
        let until = recorded_at + QUOTA_RESET_TRUST_MAX_SECS + 3_600;
        health.seed(
            "p1",
            Block {
                kind: BlockKind::EndpointAuth,
                until: Some(until),
                detail: "seeded evidence".to_owned(),
                status: None,
                recorded_at,
            },
        );
        ctx.endpoint_health = Some(health.clone());

        let outcome = acquire_once(&mut ctx).await;

        let AcquireResult::Stop { message } = outcome else {
            panic!("an endpoint-level fact with no sibling stops the round");
        };
        assert_eq!(display_until(recorded_at, Some(until)), None);
        assert!(
            message.contains(&format!(
                "upstream said it reopens at {until} (unix seconds)"
            )),
            "{message}"
        );
    }

    /// The other case: a chain that does exist, with every candidate ahead
    /// marked. The note names the marking.
    #[tokio::test]
    async fn a_fact_stop_with_every_candidate_marked_names_the_marking() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded(BlockKind::EndpointAuth, None));
        health.seed("p2", seeded(BlockKind::EndpointAuth, None));
        ctx.endpoint_health = Some(health.clone());

        let outcome = acquire_once(&mut ctx).await;

        let AcquireResult::Stop { message } = outcome else {
            panic!("a marked chain stops the round");
        };
        assert!(
            message.contains("marked in the shared fact table"),
            "{message}"
        );
        assert_eq!(backend.take_requests().len(), 0, "nothing is asked");
    }

    /// The wake's own attempt is the only one the table lets through: a
    /// wake-triggered execution sends once even against a marked endpoint, and the
    /// attempt it would make next is withheld. The origin is spent on the first
    /// reading — a second reading that still read `External` would send again.
    #[tokio::test]
    async fn the_wake_origin_is_spent_on_the_first_attempt() {
        let backend = RecordingBackend::new();
        backend.queue_dropped_stream(
            vec![just_llm_client::types::generation::GenerationEvent::Text {
                delta: "partial".to_owned(),
            }],
            just_llm_client::TransportError::InvalidResponse(
                "stream closed before the terminal event".to_owned(),
            ),
        );
        // A second stream is queued so a second send would succeed: the request
        // count pins the claim that no second send happens.
        backend.queue_stream("resp-2");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy {
                base_delay: std::time::Duration::from_millis(1),
                max_delay: std::time::Duration::from_millis(5),
                max_retries: 2,
                ..RetryPolicy::default()
            },
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded(BlockKind::Quota, None));
        ctx.endpoint_health = Some(health.clone());
        ctx.attempt_origin = AttemptOrigin::External;

        let outcome = acquire_once(&mut ctx).await;

        assert_eq!(
            backend.take_requests().len(),
            1,
            "the wake's own attempt is served, the attempt the table marks is not"
        );
        assert!(
            matches!(
                outcome,
                AcquireResult::Outcome(AgentOutcome::QuotaBlocked(_))
            ),
            "the fact the second reading sees parks the round"
        );
    }

    /// The account hint a park carries is the fact holder's, not the asker's: two
    /// endpoints on one credential answer the same fact, and the hint has to name
    /// the credential the fact came through.
    #[tokio::test]
    async fn a_park_hint_names_the_fact_holder_not_the_asker() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        // p2 is the endpoint in use; the fact was recorded through p1, on the
        // credential the two share.
        let mut ctx = ctx_from_source(
            vec![profile("a", "p2", 100_000), profile("b", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded(BlockKind::Quota, None));
        health.share_account("p1", "p2");
        ctx.endpoint_health = Some(health.clone());

        let outcome = acquire_once(&mut ctx).await;

        let AcquireResult::Outcome(AgentOutcome::QuotaBlocked(info)) = outcome else {
            panic!("an account-level fact parks the round");
        };
        assert_eq!(info.endpoint, "p2", "the endpoint in use is the asker");
        assert_eq!(
            info.account_hint,
            account_hint("p1"),
            "the hint names the credential the fact came through"
        );
        assert_ne!(
            info.account_hint,
            account_hint("p2"),
            "not the asker's own id"
        );
        assert_eq!(backend.take_requests().len(), 0, "nothing is asked");
    }

    /// A mid-stream quota rejection leaves the same observation record the send
    /// path does: the class decides it, the upstream's reset moment rides it, and
    /// no wait follows.
    #[tokio::test]
    async fn a_mid_stream_quota_rejection_leaves_an_observation_record() {
        let backend = RecordingBackend::new();
        backend.queue_dropped_stream(
            vec![just_llm_client::types::generation::GenerationEvent::Text {
                delta: "partial".to_owned(),
            }],
            just_llm_client::TransportError::HttpStatus {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                body: "{\"error\":\"slow down\",\"retry_after\":120}".to_owned(),
            },
        );
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        ctx.endpoint_health = Some(health.clone());
        let before = timefmt::now_epoch();

        let outcome = acquire_once(&mut ctx).await;

        assert!(
            matches!(
                outcome,
                AcquireResult::Outcome(AgentOutcome::QuotaBlocked(_))
            ),
            "the mid-stream rejection parks with the structured reason"
        );
        let records = ctx.store.lock().await.retry_log.clone();
        assert_eq!(records.len(), 1, "one rejection, one observation");
        assert!(matches!(records[0].kind, RetryKind::RateLimit));
        assert_eq!(records[0].delay_secs, 0.0, "no wait follows a quota stop");
        let reset = records[0].quota_reset.expect("the hint rides the record");
        assert!(reset >= before + 120, "{reset}");
        assert_eq!(records[0].endpoint.as_deref(), Some("p1"));
    }

    // -- a fact that ends a round: what the gateway hears ------------------

    /// A fact stop on endpoint credentials tells the gateway what a rejection
    /// tells it. The fact path has no error to walk for a status, so the status
    /// rides the fact itself — and a fact stores no upstream error code.
    #[tokio::test]
    async fn an_endpoint_auth_fact_signals_the_gateway_when_it_ends_the_round() {
        let backend = RecordingBackend::new();
        // A stream is queued so a send would succeed: the request count pins the
        // claim that the marked send never happens.
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded_with_status(BlockKind::EndpointAuth, None, 401));
        ctx.endpoint_health = Some(health.clone());
        let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.gateway_signals = Some(signal_tx);

        let outcome = acquire_once(&mut ctx).await;

        assert!(
            matches!(outcome, AcquireResult::Stop { .. }),
            "an endpoint-level fact with no sibling stops the round"
        );
        let signal = signal_rx.try_recv().expect("the fact reaches the gateway");
        assert_eq!(signal.status, 401);
        assert_eq!(signal.code, None, "a fact stores no upstream error code");
        assert!(signal_rx.try_recv().is_err(), "one round, one signal");
        assert_eq!(
            backend.take_requests().len(),
            0,
            "the marked send never happens"
        );
    }

    /// An endpoint-level verdict the gateway does not act on stays silent: 404
    /// says the endpoint is gone, not that its credentials were rejected.
    #[tokio::test]
    async fn an_endpoint_auth_fact_with_a_status_the_gateway_ignores_stays_silent() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded_with_status(BlockKind::EndpointAuth, None, 404));
        ctx.endpoint_health = Some(health.clone());
        let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.gateway_signals = Some(signal_tx);

        let outcome = acquire_once(&mut ctx).await;

        assert!(
            matches!(outcome, AcquireResult::Stop { .. }),
            "the fact still stops the round"
        );
        assert!(signal_rx.try_recv().is_err(), "404 never signals");
    }

    /// An account-level fact never signals, whatever status it carries: a quota
    /// block arrives as a 403 too, and the level — not the status — is what says
    /// this is not an auth rejection. The walk it drives finds no buildable
    /// candidate, so the round parks and the gateway hears nothing.
    #[tokio::test]
    async fn an_account_level_fact_never_signals_the_gateway() {
        let backend = RecordingBackend::new();
        // p2 is declared but absent from the source, so building it fails: the
        // fact-driven walk ends here rather than on another endpoint's answer.
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded_with_status(BlockKind::Quota, None, 403));
        ctx.endpoint_health = Some(health.clone());
        let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.gateway_signals = Some(signal_tx);

        let outcome = acquire_once(&mut ctx).await;

        assert!(
            matches!(
                outcome,
                AcquireResult::Outcome(AgentOutcome::QuotaBlocked(_))
            ),
            "an unbuildable candidate leaves nothing to switch to, so the block parks"
        );
        assert!(
            signal_rx.try_recv().is_err(),
            "a quota block is not an auth rejection, whatever status it carries"
        );
    }

    /// A fact the chain recovers from stays silent: the signal is for a round that
    /// ended on a rejection, not for one that walked past a marked endpoint.
    #[tokio::test]
    async fn a_fact_the_chain_recovers_from_stays_silent() {
        let backend = RecordingBackend::new();
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            RetryPolicy::default(),
        )
        .await;
        let health = Arc::new(RecordingHealth::default());
        health.seed("p1", seeded_with_status(BlockKind::EndpointAuth, None, 401));
        health.share_account("p1", "p2");
        ctx.endpoint_health = Some(health.clone());
        let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.gateway_signals = Some(signal_tx);

        let outcome = acquire_once(&mut ctx).await;

        assert!(
            matches!(outcome, AcquireResult::Consumed(_)),
            "the sibling serves the turn"
        );
        assert_eq!(
            backend.take_requests().len(),
            1,
            "only the sibling is asked"
        );
        assert!(
            signal_rx.try_recv().is_err(),
            "a walk that recovered says nothing to the gateway"
        );
    }

    // -- acquisition-level shape repeats -----------------------------------

    /// A shape repeat is an endpoint-level verdict, and the profile it advances to
    /// starts a fresh ledger: the same shape failing there is that episode's first,
    /// so it retries in place instead of stopping. The send count pins it — a
    /// carried-over shape slot would end the round after three sends.
    #[tokio::test]
    async fn a_shape_repeat_switches_endpoints_and_the_next_episode_counts_its_own() {
        let backend = RecordingBackend::new();
        for _ in 0..3 {
            backend.queue_error(BackendError::provider(
                "recording",
                TransportError::InvalidResponse("empty body".into()),
            ));
        }
        backend.queue_stream("resp-1");
        let source = Arc::new(MapSource(HashMap::from([
            ("p1".to_string(), backend.clone() as Arc<dyn LlmBackend>),
            ("p2".to_string(), backend.clone() as Arc<dyn LlmBackend>),
        ])));
        let policy = RetryPolicy {
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
            ..RetryPolicy::default()
        };
        let mut ctx = ctx_from_source(
            vec![profile("a", "p1", 100_000), profile("b", "p2", 100_000)],
            source,
            policy,
        )
        .await;

        let outcome = acquire_once(&mut ctx).await;

        assert!(
            matches!(outcome, AcquireResult::Consumed(_)),
            "the second endpoint serves the turn"
        );
        let models: Vec<String> = backend
            .take_requests()
            .iter()
            .map(|request| request.model.clone())
            .collect();
        assert_eq!(
            models,
            vec!["a-model", "a-model", "b-model", "b-model"],
            "the repeat switches endpoints, and the next episode spends its own shape slot"
        );
        assert_eq!(
            ctx.episode.attempts(),
            2,
            "the new episode counts its own sends"
        );
    }

    /// The same shape failing twice mid-stream ends the round: the first drop
    /// retries in place, the repeat is endpoint-level, and a single-profile set has
    /// nowhere to walk. The send count pins the stop — a third send would mean the
    /// repeat had been retried as well.
    #[tokio::test]
    async fn a_mid_stream_shape_repeat_stops_the_round() {
        let backend = RecordingBackend::new();
        for _ in 0..2 {
            backend.queue_dropped_stream(
                vec![GenerationEvent::Text {
                    delta: "partial".to_owned(),
                }],
                TransportError::InvalidResponse("stream closed before the terminal event".into()),
            );
        }
        let source = Arc::new(MapSource(HashMap::from([(
            "p1".to_string(),
            backend.clone() as Arc<dyn LlmBackend>,
        )])));
        let policy = RetryPolicy {
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
            ..RetryPolicy::default()
        };
        let mut ctx = ctx_from_source(vec![profile("a", "p1", 100_000)], source, policy).await;

        let outcome = acquire_once(&mut ctx).await;

        let AcquireResult::Stop { message } = outcome else {
            panic!("a repeated mid-stream shape with no candidate left stops the round");
        };
        assert!(
            message.contains("the response shape failed twice in this episode"),
            "the stop names the repeat: {message}"
        );
        assert_eq!(
            backend.take_requests().len(),
            2,
            "the first drop retries in place; the repeat stops without a third send"
        );
    }
}
