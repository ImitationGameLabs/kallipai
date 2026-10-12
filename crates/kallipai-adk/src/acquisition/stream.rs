//! Stream consumption and the within-set acquisition loop.
//!
//! Consume an SSE stream, retry a mid-stream transport drop in place, and walk the
//! failover chain when an endpoint or the retry budget gives out. The chain advance
//! itself lives in `super::failover`; this module owns the loop that calls it and the
//! request rebuild around it.

use super::facts::{
    emit_fact_signal, fact_chain_note, fact_error, fact_stop_message, log_fact_skip,
    quota_blocked_payload, table_reading,
};
use super::failover::{
    Exhaustion, FailedSend, FailoverStep, LOCAL_CONFIG_NOTE, step_failover, stop_message,
};
use super::{flush_retry_records, retry_keep_since};

use crate::agent_task::AgentContext;
use crate::context::{compose_context, message_has_images, strip_message_images};
use crate::event::{AgentEvent, AgentOutcome};
use crate::retry::decision::{self, Attempt, AttemptOrigin, Decision, Phase, StopKind};
use crate::stream_accumulator::ToolCallAccumulator;
use anyhow::Error;
use futures_util::StreamExt;
use just_llm_client::types::generation::{
    GenerationEvent, GenerationRequest, Message, ToolCall, ToolChoice, ToolChoiceMode,
    ToolDefinition,
};
use just_llm_client::{BackendError, ConversationStream, provider_rejection};
use kallipai_common::retry::RetryRecord;
use kallipai_common::timefmt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

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
pub(super) fn fatal_provider_status(error: &Error) -> Option<reqwest::StatusCode> {
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
                    FailedSend {
                        trigger: fact,
                        reason: rendered,
                        exhaustion,
                    },
                    tx,
                    round_cancel,
                    round,
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
                    FailedSend {
                        trigger: spent.into(),
                        reason: rendered,
                        exhaustion: Exhaustion::Transient,
                    },
                    tx,
                    round_cancel,
                    round,
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
                    FailedSend {
                        trigger: e.into(),
                        reason,
                        exhaustion: Exhaustion::Transient,
                    },
                    tx,
                    round_cancel,
                    round,
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
                    FailedSend {
                        trigger: error.into(),
                        reason: rendered,
                        exhaustion,
                    },
                    tx,
                    round_cancel,
                    round,
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
