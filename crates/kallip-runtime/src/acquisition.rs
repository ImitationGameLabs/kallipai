//! LLM stream acquisition for the agent round loop.
//!
//! Owns the within-tier acquisition loop: consume an SSE stream, retry mid-stream
//! transport drops in place, and advance the failover chain when an endpoint or the
//! retry budget gives out. The [`crate::failover::FailoverState`] state machine stays
//! pure in its own module — this module drives it, and `advance_failover` below is the
//! sole driver of chain advancement. The round loop in `crate::runner` calls
//! [`acquire_stream`] and dispatches on the result.

use anyhow::Error;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::agent_task::AgentContext;
use crate::context::{CompactOutcome, compose_context, summarize_and_evict};
use crate::event::{AgentEvent, AgentOutcome};
use crate::failover::FailoverOutcome;
use crate::stream_accumulator::ToolCallAccumulator;
use just_llm_client::types::chat::{
    ChatMessage, ChatToolCall, StreamOptions, ToolChoice, ToolChoiceMode, ToolDefinition,
};
use kallip_common::protocol::FailoverChainExhaustion;
use kallip_common::retry::RetryRecord;

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
    /// is abandoned (already-emitted deltas are void); the caller retries from scratch. Carries the
    /// error so the caller can drive retry/failover and surface a diagnostic.
    Transient(just_llm_client::TransportError),
}

/// Data accumulated from a completed LLM response stream.
pub(crate) struct StreamConsumed {
    pub(crate) content: String,
    pub(crate) reasoning: String,
    pub(crate) tool_calls: Vec<ChatToolCall>,
    pub(crate) usage: Option<just_llm_client::types::chat::Usage>,
}

/// Consume an SSE stream, accumulating content, reasoning, tool calls, and usage.
///
/// Takes ownership of the stream and pins it internally.
/// Returns `Cancelled` if the cancellation token fires mid-stream.
async fn consume_stream(
    stream: just_llm_client::ChatCompletionStream,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    cancel: &CancellationToken,
) -> StreamOutcome {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_acc = ToolCallAccumulator::new();
    let mut response_usage: Option<just_llm_client::types::chat::Usage> = None;

    tokio::pin!(stream);
    loop {
        tokio::select! {
            chunk_result = stream.next() => {
                let chunk = match chunk_result {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => {
                        // Mid-stream transport drop (connection reset, h2 error, premature EOF, ...).
                        // The partial content already emitted via deltas is void; the caller retries
                        // from scratch and emits a `StreamReset` so downstream folds/discards it.
                        info!(
                            "LLM stream dropped mid-stream: {}",
                            crate::llm_error::render_error(&e)
                        );
                        return StreamOutcome::Transient(e);
                    }
                    None => break,
                };
                let choice = match chunk.choices.first() {
                    Some(c) => c,
                    None => continue,
                };

                if let Some(delta) = &choice.delta.content {
                    content.push_str(delta);
                    tx.send(AgentEvent::AssistantContentDelta { delta: delta.clone() })
                        .await
                        .ok();
                }

                if let Some(delta) = &choice.delta.reasoning_content {
                    reasoning.push_str(delta);
                    tx.send(AgentEvent::ReasoningDelta { delta: delta.clone() })
                        .await
                        .ok();
                }

                if let Some(deltas) = &choice.delta.tool_calls {
                    for tc in deltas {
                        tool_acc.push(tc);
                    }
                }

                if let Some(usage) = chunk.usage.clone() {
                    response_usage = Some(usage);
                }
            }
            _ = cancel.cancelled() => {
                tracing::info!("LLM stream cancelled mid-stream");
                return StreamOutcome::Cancelled;
            }
        }
    }

    StreamOutcome::Completed(StreamConsumed {
        content,
        reasoning,
        tool_calls: tool_acc.finish(),
        usage: response_usage,
    })
}

// ---------------------------------------------------------------------------
// Within-tier acquisition
// ---------------------------------------------------------------------------

/// Outcome of the within-tier failover acquisition loop.
pub(crate) enum AcquireResult {
    /// A stream was acquired AND fully consumed — proceed to post-stream budgeting / tool calls.
    Consumed(StreamConsumed),
    /// A terminal round outcome (chain exhausted / cancelled / budget exceeded).
    Outcome(AgentOutcome),
    /// A request-level error — the round errors.
    Error(Error),
}
/// Within-tier failover acquisition: rebuild the request per profile, retry, and on a `Failover`
/// outcome advance the chain. Self-contained — owns `retry_records` and flushes them on every
/// early-exit arm and after a successful break.
///
/// On `Failover` (endpoint-level failure, or transient retries exhausted) the acquisition loop
/// advances to the next profile in the tier, rebuilds the client, and retries the same turn. On `Fatal`
/// (request-level) it errors the round. `profile_idx` only moves forward and sticks for the
/// agent's lifetime (resets to 0 on spawn/restore). The inner `tokio::select!` cancel arm stays
/// inside this function so a cancel during the retry backoff flushes and short-circuits here.
pub(crate) async fn acquire_stream(
    ctx: &mut AgentContext,
    mut messages: Vec<ChatMessage>,
    tools: Vec<ToolDefinition>,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    round_cancel: &CancellationToken,
    round: usize,
) -> AcquireResult {
    let mut retry_records = Vec::new();
    let consumed = loop {
        let endpoint_id = ctx.failover.current_profile().endpoint.clone();
        let prior_retries = count_recent_retries(ctx, &endpoint_id).await;
        let request = ctx
            .client
            .create_request(messages.clone())
            .with_tools(tools.clone())
            .with_tool_choice(ToolChoice::Mode(ToolChoiceMode::Auto));
        let mut request = request;
        request.stream = Some(true);
        request.stream_options = Some(StreamOptions {
            include_usage: Some(true),
        });

        let result = {
            let fut = crate::retry::stream_with_retry(
                crate::retry::RetryCall {
                    client: &ctx.client,
                    request,
                    policy: &ctx.config.retry_policy,
                    round,
                    prior_retries,
                    endpoint_id: &endpoint_id,
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
            Ok(stream) => stream,
            Err(crate::retry::RequestFailure::Fatal(e)) => {
                flush_retry_records(ctx, &mut retry_records).await;
                return AcquireResult::Error(e.into());
            }
            Err(crate::retry::RequestFailure::Failover(e)) => {
                // Flush this endpoint's retries (tagged with its endpoint id for per-endpoint
                // budget scoping) before advancing.
                flush_retry_records(ctx, &mut retry_records).await;
                let reason = crate::llm_error::render_error(&e);
                match step_failover(ctx, &mut messages, e.into(), reason, tx, round_cancel).await {
                    FailoverStep::Advanced => continue,
                    FailoverStep::Done(result) => return result,
                }
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
        // a `StreamReset` event) and the request is re-sent from scratch.
        match consume_stream(stream, tx, round_cancel).await {
            StreamOutcome::Completed(c) => {
                flush_retry_records(ctx, &mut retry_records).await;
                break c;
            }
            StreamOutcome::Cancelled => {
                flush_retry_records(ctx, &mut retry_records).await;
                return AcquireResult::Outcome(AgentOutcome::Cancelled);
            }
            StreamOutcome::Transient(e) => {
                // This attempt's prepare/send retries (if any) were buffered locally; persist
                // them first so they count toward the shared per-endpoint budget before we
                // decide whether a mid-stream retry remains within it.
                flush_retry_records(ctx, &mut retry_records).await;
                let prior_retries = count_recent_retries(ctx, &endpoint_id).await;
                let policy = &ctx.config.retry_policy;
                let attempt = prior_retries + 1;
                let error_msg = crate::llm_error::render_error(&e);

                if prior_retries >= policy.max_retries {
                    // Budget exhausted: the endpoint keeps dropping mid-stream. Void the partial
                    // downstream first (no backoff — failover is immediate; `delay_secs: 0.0`),
                    // then treat as transient-exhausted and advance the failover chain (a
                    // different profile/endpoint may hold a healthier connection). `attempt`
                    // exceeds `max_attempts` here, honestly signalling the budget is blown.
                    tx.try_send(AgentEvent::StreamReset {
                        error: error_msg,
                        attempt,
                        max_attempts: policy.max_retries,
                        delay_secs: 0.0,
                    })
                    .ok();
                    let backend_err =
                        just_llm_client::BackendError::provider(ctx.client.family(), e);
                    let reason = crate::llm_error::render_error(&backend_err);
                    match step_failover(
                        ctx,
                        &mut messages,
                        backend_err.into(),
                        reason,
                        tx,
                        round_cancel,
                    )
                    .await
                    {
                        FailoverStep::Advanced => continue,
                        FailoverStep::Done(result) => return result,
                    }
                }

                // Within budget: tell downstream to fold/discard the partial it already rendered,
                // back off, record the retry, then re-acquire. Mirrors the backoff/emit/record
                // shape of `retry::stream_with_retry`'s `Attempt::Retry` arm. Two intentional
                // omissions vs that arm: a mid-stream drop carries no `Retry-After` header (no
                // floor), and the `retry_timeout` deadline is scoped per `stream_with_retry` call
                // (reset on each re-entry), so there is no remaining-deadline cap to apply here.
                // The backoff exponent is `prior_retries` (cumulative across the endpoint's
                // in-window retries), so sustained flakiness lengthens the wait.
                let delay = crate::retry::backoff_delay(policy, prior_retries);
                let delay_secs = delay.as_secs_f64();
                tx.try_send(AgentEvent::StreamReset {
                    error: error_msg.clone(),
                    attempt,
                    max_attempts: policy.max_retries,
                    delay_secs,
                })
                .ok();
                let record = RetryRecord {
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    round,
                    attempt,
                    max_attempts: policy.max_retries,
                    error: error_msg,
                    delay_secs,
                    endpoint: Some(endpoint_id.clone()),
                };
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {
                        ctx.store.lock().await.retry_log.push(record);
                        ctx.persist().await;
                    }
                    _ = round_cancel.cancelled() => {
                        return AcquireResult::Outcome(AgentOutcome::Cancelled);
                    }
                }
                // Loop continues; prior_retries is recomputed (now incremented by the record
                // just persisted) and the request is re-sent.
            }
        }
    };
    AcquireResult::Consumed(consumed)
}

/// Count this endpoint's recent retries (within `retry_timeout`, across rounds) persisted in the
/// store. Rate limits are endpoint-scoped, so the per-endpoint budget is shared: two profiles on
/// the same endpoint draw on the same quota, and a successor counts its predecessor's in-window
/// retries.
async fn count_recent_retries(ctx: &mut AgentContext, endpoint_id: &str) -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let window_secs = ctx.config.retry_policy.retry_timeout.as_secs();
    ctx.store
        .lock()
        .await
        .retry_log
        .iter()
        .filter(|r| r.endpoint.as_deref() == Some(endpoint_id) && r.timestamp + window_secs > now)
        .count() as u32
}

/// Persist any buffered retry records for this endpoint into the store. A no-op when empty.
async fn flush_retry_records(ctx: &mut AgentContext, retry_records: &mut Vec<RetryRecord>) {
    if retry_records.is_empty() {
        return;
    }
    ctx.store
        .lock()
        .await
        .retry_log
        .extend(std::mem::take(retry_records));
    ctx.persist().await;
}

// ---------------------------------------------------------------------------
// Failover driving
// ---------------------------------------------------------------------------

/// One step of within-tier failover, shared by the prepare/send/parse `Failover` arm and the
/// mid-stream-drop budget-exhausted path so both entry points behave identically.
enum FailoverStep {
    /// The chain advanced to a new profile; the caller re-loops with the rebound `messages`.
    Advanced,
    /// A terminal acquire result (chain exhausted / cancelled / budget exceeded).
    Done(AcquireResult),
}

/// Drive within-tier failover for `trigger`: advance the chain, emit a `Failover` event on
/// advance, and map the [`FailoverOutcome`] to a [`FailoverStep`]. `reason` is the operator-facing
/// diagnostic captured from `trigger` before it is moved into [`advance_failover`].
async fn step_failover(
    ctx: &mut AgentContext,
    messages: &mut Vec<ChatMessage>,
    trigger: Error,
    reason: String,
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
            warn!(from = %from, to = %to, reason = %reason, "within-tier failover");
            tx.send(AgentEvent::Failover { from, to, reason })
                .await
                .ok();
            FailoverStep::Advanced
        }
        FailoverOutcome::ChainExhausted { reason, trigger } => {
            // Chain exhaustion is a defined round-end (sibling of MaxRoundsExceeded), surfaced
            // as a distinguishable terminal outcome rather than a generic `Err`. `trigger` is an
            // `anyhow::Error`; `render_error` de-duplicates its source chain and surfaces any HTTP
            // error body (`as_ref` is required — `anyhow::Error` does not impl `std::error::Error`).
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

/// Advance the within-tier failover chain on a terminal endpoint failure (`trigger`).
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
    prior_messages: Vec<ChatMessage>,
    trigger: anyhow::Error,
    round_cancel: &CancellationToken,
) -> FailoverOutcome {
    // Honor a cancel that raced in between the failover decision and the chain advance — takes
    // precedence over chain-exhaustion.
    if round_cancel.is_cancelled() {
        return FailoverOutcome::Cancelled;
    }
    // No candidate ahead — single-profile tier, or already at the chain tail. Distinguish the
    // two: a single-profile tier means failover was never configured, while a multi-profile
    // tier at its tail means the chain was advanced through and now the last profile failed.
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
    // credentials/endpoint config. (See the coalescing rule on `FailoverChainExhaustion`.)
    let mut skipped_infeasible = false;
    while let Some(candidate) = ctx.failover.candidate_profile(offset) {
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
    } else {
        FailoverChainExhaustion::AllCandidatesUnbuildable
    };
    FailoverOutcome::ChainExhausted { reason, trigger }
}

/// Re-apply the active profile's declared context window to the config and re-sync the store's
/// pinned-budget guard. Called after a failover advance swaps to a profile that may declare a
/// different window (within-tier heterogeneous windows are supported).
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
