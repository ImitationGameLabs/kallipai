//! Agent round execution loop.
//!
//! Owns the per-round orchestration: drain interjections, gate the context budget, acquire the
//! LLM stream (with within-tier failover), consume it, and execute tool calls. The context-budget
//! primitives (estimation, warning injection, compaction) live in [`crate::context`]; the
//! [`crate::failover::FailoverState`] state machine and the low-level [`ToolCallAccumulator`] are
//! kept pure in their own modules — this module only drives them.

use std::time::Duration;

use anyhow::{Error, Result, bail};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::agent_task::AgentContext;
use crate::approval::format_approval_notifications;
use crate::context::{
    CompactOutcome, check_progressive_warnings, check_token_budget_warnings, compose_context,
    estimate_context_tokens, summarize_and_evict,
};
use crate::event::{AgentEvent, AgentOutcome};
use crate::failover::FailoverOutcome;
use crate::stream_accumulator::ToolCallAccumulator;
use just_agent_common::protocol::FailoverChainExhaustion;
use just_llm_client::types::chat::{
    ChatMessage, ChatToolCall, StreamOptions, ToolCallsMessage, ToolChoice, ToolChoiceMode,
    ToolDefinition,
};

// ---------------------------------------------------------------------------
// Stream consumption
// ---------------------------------------------------------------------------

/// Outcome of consuming an LLM response stream.
enum StreamOutcome {
    /// The stream was cancelled mid-stream.
    Cancelled,
    /// The stream completed normally.
    Completed(StreamConsumed),
}

/// Data accumulated from a completed LLM response stream.
struct StreamConsumed {
    content: String,
    reasoning: String,
    tool_calls: Vec<ChatToolCall>,
    usage: Option<just_llm_client::types::chat::Usage>,
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
                        warn!("stream chunk error: {e:#}");
                        break;
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
// Round-loop control-flow signals
// ---------------------------------------------------------------------------

/// A budget-gate phase's verdict on whether the round loop should proceed. Makes a phase's
/// early-exit contract (`continue` / `return`) explicit at the call site.
enum BudgetAction {
    /// Re-compose context and re-enter the loop (a warning was injected or context was compacted).
    Recompose,
    /// Exit the loop with this terminal outcome.
    Return(AgentOutcome),
    /// Fall through to the next phase.
    Proceed,
}

/// Outcome of the within-tier failover acquisition loop.
enum AcquireResult {
    /// A live stream was acquired — proceed to consume it.
    Stream(just_llm_client::ChatCompletionStream),
    /// A terminal round outcome (chain exhausted / cancelled / budget exceeded).
    Outcome(AgentOutcome),
    /// A request-level error — the round errors.
    Error(Error),
}

/// Outcome of executing the assistant's tool calls.
enum ToolExecResult {
    /// The assembled turn messages (the assistant tool-call message + tool results).
    Messages(Vec<ChatMessage>),
    /// Cancelled mid-execution; partial results are dropped (mirrors the original early-return).
    Cancelled,
}

// ---------------------------------------------------------------------------
// Agent round loop
// ---------------------------------------------------------------------------

/// Run the agent round loop until completion or max rounds.
pub(crate) async fn run_agent_rounds(
    ctx: &mut AgentContext,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
    round_cancel: &CancellationToken,
) -> Result<AgentOutcome> {
    let tool_timeout = Duration::from_secs(ctx.config.tool_timeout_secs);

    for round in 0..ctx.config.max_tool_rounds {
        drain_interjections(ctx, prompt_rx).await;

        // -- Pre-call token budget check (shared tree-wide counter) --
        let snap = ctx.token_budget.snapshot();
        if snap.is_exceeded() {
            return Ok(AgentOutcome::TokenBudgetExceeded {
                consumed: snap.consumed,
                budget: snap.budget,
            });
        }

        // -- Approval notification injection --
        let notifications = ctx.approvals.lock().await.drain_notifications();
        if !notifications.is_empty() {
            let msg = format_approval_notifications(&notifications);
            ctx.record_turn(vec![ChatMessage::user(&msg)]).await;
        }

        // -- Context composition and token estimation --
        let messages = compose_context(ctx.store.clone()).await;
        let tools = ctx.store.lock().await.tool_definitions().to_vec();
        let prompt_tokens = {
            let system_prompt = ctx.client.system_prompt().map(str::to_owned);
            let estimate = estimate_context_tokens(
                &ctx.client,
                &ctx.store,
                &messages,
                &tools,
                system_prompt.as_deref(),
            );
            tokio::select! {
                result = estimate => match result {
                    Ok(tokens) => tokens,
                    Err(e) => {
                        warn!("token estimation failed, sending request anyway: {e:#}");
                        0
                    }
                },
                _ = round_cancel.cancelled() => return Ok(AgentOutcome::Cancelled),
            }
        };

        match enforce_pre_call_budget(ctx, prompt_tokens, round_cancel).await {
            BudgetAction::Recompose => continue,
            BudgetAction::Return(outcome) => return Ok(outcome),
            BudgetAction::Proceed => {}
        }

        // -- Within-tier failover acquisition --
        let stream = match acquire_stream(ctx, messages, tools, tx, round_cancel, round).await {
            AcquireResult::Stream(s) => s,
            AcquireResult::Outcome(outcome) => return Ok(outcome),
            AcquireResult::Error(e) => return Err(e),
        };

        // -- Stream consumption --
        let consumed = match consume_stream(stream, tx, round_cancel).await {
            StreamOutcome::Cancelled => return Ok(AgentOutcome::Cancelled),
            StreamOutcome::Completed(c) => c,
        };

        match enforce_post_stream_budget(ctx, consumed.usage.as_ref()).await {
            BudgetAction::Recompose => continue,
            BudgetAction::Return(outcome) => return Ok(outcome),
            BudgetAction::Proceed => {}
        }

        // -- Finished? --
        if consumed.tool_calls.is_empty() {
            if !consumed.content.is_empty() {
                return Ok(AgentOutcome::Finished {
                    content: consumed.content,
                });
            }
            bail!("assistant returned neither tool calls nor final content");
        }

        // -- Tool execution --
        let turn_messages =
            match execute_tool_calls(ctx, tx, consumed, tool_timeout, round_cancel).await {
                ToolExecResult::Cancelled => return Ok(AgentOutcome::Cancelled),
                ToolExecResult::Messages(msgs) => msgs,
            };

        ctx.record_turn(turn_messages).await;
        ctx.persist().await;
    }

    Ok(AgentOutcome::MaxRoundsExceeded)
}

/// Consume queued interjections (prompts/commands) and record them as a single turn.
async fn drain_interjections(
    ctx: &mut AgentContext,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
) {
    let mut interjected = Vec::new();
    while let Ok(text) = prompt_rx.try_recv() {
        interjected.push(text);
    }
    if !interjected.is_empty() {
        let msg = interjected
            .iter()
            .map(|t| format!("[Interjected message]\n{t}\n[/Interjected message]"))
            .collect::<Vec<_>>()
            .join("\n");
        ctx.record_turn(vec![ChatMessage::user(&msg)]).await;
        info!(count = interjected.len(), "injected interjected messages");
    }
}

/// Progressive-warning + auto-compact gate run before the LLM request. `Recompose` when a warning
/// was injected or context was compacted (the loop re-composes); `Return` on budget exhaustion or
/// a raced-in cancel; `Proceed` to send the request.
async fn enforce_pre_call_budget(
    ctx: &mut AgentContext,
    prompt_tokens: usize,
    round_cancel: &CancellationToken,
) -> BudgetAction {
    if prompt_tokens == 0 {
        return BudgetAction::Proceed;
    }
    let effective_budget = ctx.config.effective_budget();
    let usage_pct = prompt_tokens * 100 / effective_budget;

    // Phase 1: Progressive warnings.
    if check_progressive_warnings(ctx, usage_pct, effective_budget).await {
        return BudgetAction::Recompose;
    }

    // Phase 2: Auto-compact at the highest threshold.
    let auto_threshold = ctx.config.auto_compact_threshold() as usize;
    if usage_pct >= auto_threshold {
        info!(
            prompt_tokens,
            context_window = ctx.config.context_window_tokens,
            "context exceeds budget"
        );
        match summarize_and_evict(ctx).await {
            Ok(CompactOutcome::Compacted) => return BudgetAction::Recompose,
            Ok(CompactOutcome::NothingToCompact) => {} // fall through
            Ok(CompactOutcome::BudgetExceeded { consumed, budget }) => {
                return BudgetAction::Return(AgentOutcome::TokenBudgetExceeded {
                    consumed,
                    budget,
                });
            }
            Err(e) => warn!("summarize_and_evict failed: {e:#}"),
        }
        if round_cancel.is_cancelled() {
            return BudgetAction::Return(AgentOutcome::Cancelled);
        }
    }

    BudgetAction::Proceed
}

/// Within-tier failover acquisition: rebuild the request per profile, retry, and on a `Failover`
/// outcome advance the chain. Self-contained — owns `retry_records` and flushes them on every
/// early-exit arm and after a successful break.
///
/// On `Failover` (endpoint-level failure, or transient retries exhausted) the runner advances to
/// the next profile in the tier, rebuilds the client, and retries the same turn. On `Fatal`
/// (request-level) it errors the round. `profile_idx` only moves forward and sticks for the
/// agent's lifetime (resets to 0 on spawn/restore). The inner `tokio::select!` cancel arm stays
/// inside this function so a cancel during the retry backoff flushes and short-circuits here.
async fn acquire_stream(
    ctx: &mut AgentContext,
    mut messages: Vec<ChatMessage>,
    tools: Vec<ToolDefinition>,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    round_cancel: &CancellationToken,
    round: usize,
) -> AcquireResult {
    let mut retry_records = Vec::new();
    let stream = loop {
        let endpoint_id = ctx.failover.current_profile().endpoint.clone();
        // Per-endpoint retry budget: rate limits are endpoint-scoped, so only this endpoint's
        // recent retries (within retry_timeout, across rounds) count. Two profiles sharing one
        // endpoint share one budget — so after advancing, a successor on the same endpoint
        // counts its predecessor's in-window retries too (correct: both draw on the same rate-
        // limit quota).
        let prior_retries = {
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
                .filter(|r| {
                    r.endpoint.as_deref() == Some(endpoint_id.as_str())
                        && r.timestamp + window_secs > now
                })
                .count() as u32
        };
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
                    if !retry_records.is_empty() {
                        ctx.store.lock().await.retry_log.extend(retry_records);
                        ctx.persist().await;
                    }
                    return AcquireResult::Outcome(AgentOutcome::Cancelled);
                }
            }
        };
        match result {
            Ok(stream) => break stream,
            Err(crate::retry::RequestFailure::Fatal(e)) => {
                if !retry_records.is_empty() {
                    ctx.store.lock().await.retry_log.extend(retry_records);
                    ctx.persist().await;
                }
                return AcquireResult::Error(e.into());
            }
            Err(crate::retry::RequestFailure::Failover(e)) => {
                // Flush this endpoint's retries (tagged with its endpoint id for per-endpoint
                // budget scoping) before advancing.
                if !retry_records.is_empty() {
                    ctx.store
                        .lock()
                        .await
                        .retry_log
                        .extend(std::mem::take(&mut retry_records));
                    ctx.persist().await;
                }
                // Capture the trigger reason before `e` moves into advance_failover.
                let reason = format!("{e:#}");
                match advance_failover(ctx, messages, e.into(), round_cancel).await {
                    FailoverOutcome::Advanced {
                        from,
                        to,
                        messages: new_messages,
                    } => {
                        messages = new_messages;
                        // Under skip, `from`→`to` may jump over unbuildable intermediates;
                        // those are warned inside advance_failover (not surfaced here).
                        info!(
                            from = %from, to = %to, reason = %reason,
                            "within-tier failover"
                        );
                        tx.send(AgentEvent::Failover { from, to, reason })
                            .await
                            .ok();
                    }
                    FailoverOutcome::ChainExhausted { reason, trigger } => {
                        // Chain exhaustion is a defined round-end (sibling of
                        // MaxRoundsExceeded), surfaced as a distinguishable terminal outcome
                        // rather than a generic `Err`. `run_and_report` emits the event.
                        return AcquireResult::Outcome(AgentOutcome::FailoverChainExhausted {
                            reason,
                            detail: format!("{trigger:#}"),
                        });
                    }
                    FailoverOutcome::Cancelled => {
                        return AcquireResult::Outcome(AgentOutcome::Cancelled);
                    }
                    FailoverOutcome::BudgetExceeded { consumed, budget } => {
                        return AcquireResult::Outcome(AgentOutcome::TokenBudgetExceeded {
                            consumed,
                            budget,
                        });
                    }
                }
            }
            Err(crate::retry::RequestFailure::Cancelled) => {
                // Cancel surfaced from within a retry backoff — flush this endpoint's
                // retries and short-circuit to a cancelled round. Mirrors the Fatal arm's flush.
                if !retry_records.is_empty() {
                    ctx.store.lock().await.retry_log.extend(retry_records);
                    ctx.persist().await;
                }
                return AcquireResult::Outcome(AgentOutcome::Cancelled);
            }
        }
    };
    if !retry_records.is_empty() {
        ctx.store.lock().await.retry_log.extend(retry_records);
        ctx.persist().await;
    }
    AcquireResult::Stream(stream)
}

/// Post-stream budget gate: accumulate usage, inject budget warnings, and check exhaustion.
/// `Recompose` when a warning was injected; `Return` on exhaustion; `Proceed` to handle tool calls.
async fn enforce_post_stream_budget(
    ctx: &mut AgentContext,
    usage: Option<&just_llm_client::types::chat::Usage>,
) -> BudgetAction {
    if let Some(usage) = usage {
        ctx.store.lock().await.accumulate_usage(usage);
        ctx.token_budget
            .record_usage(usage.prompt_tokens as u64, usage.completion_tokens as u64);
    }

    // Reload budget — the operator may have increased it via API mid-round.
    let snap = ctx.token_budget.snapshot();

    // Token budget warning injection (before exhaustion check).
    if check_token_budget_warnings(ctx, &snap).await {
        return BudgetAction::Recompose;
    }

    // Token budget exhaustion check (shared tree-wide counter).
    if snap.is_exceeded() {
        return BudgetAction::Return(AgentOutcome::TokenBudgetExceeded {
            consumed: snap.consumed,
            budget: snap.budget,
        });
    }

    BudgetAction::Proceed
}

/// Execute the assistant's tool calls, emitting events and assembling the turn messages. On a
/// mid-call cancel returns `Cancelled` *before* the approval-state drain (mirrors the original),
/// dropping any partial results — the caller does not record the turn.
///
/// The assistant `ToolCalls` message clones `tool_calls` before the move-iterate loop consumes
/// them, so both the recorded assistant turn and the per-call dispatch see the full set.
async fn execute_tool_calls(
    ctx: &mut AgentContext,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    consumed: StreamConsumed,
    tool_timeout: Duration,
    round_cancel: &CancellationToken,
) -> ToolExecResult {
    let mut turn_messages = vec![ChatMessage::ToolCalls(ToolCallsMessage {
        role: "assistant".into(),
        content: if consumed.content.is_empty() {
            None
        } else {
            Some(consumed.content)
        },
        name: None,
        tool_calls: consumed.tool_calls.clone(),
        reasoning_content: if consumed.reasoning.is_empty() {
            None
        } else {
            Some(consumed.reasoning)
        },
    })];

    for call in consumed.tool_calls {
        tx.send(AgentEvent::ToolCall {
            name: call.function.name.clone(),
            args: call.function.arguments.clone(),
        })
        .await
        .ok();
        let result = {
            let tool_fut = tokio::time::timeout(
                tool_timeout,
                ctx.executor
                    .execute(&call.function.name, &call.function.arguments),
            );
            tokio::select! {
                result = tool_fut => match result {
                    Ok(output) => output,
                    Err(_) => format!(
                        "tool '{}' timed out after {}s",
                        call.function.name,
                        tool_timeout.as_secs()
                    ),
                },
                _ = round_cancel.cancelled() => {
                    tracing::info!(tool = %call.function.name, "tool execution cancelled");
                    return ToolExecResult::Cancelled;
                }
            }
        };

        // Check approval state transitions (single lock acquisition).
        let (committed, redeemed, cancelled) = {
            let mut d = ctx.approvals.lock().await;
            (
                d.take_last_committed(),
                d.take_last_redeemed(),
                d.take_last_cancelled(),
            )
        };
        if let Some(info) = committed {
            let arguments =
                serde_json::from_str(&info.args_json).unwrap_or(serde_json::Value::Null);
            tx.send(AgentEvent::ApprovalCommitted {
                id: info.id,
                tool_name: info.tool_name,
                arguments,
                commit_reason: info.commit_reason,
            })
            .await
            .ok();
        }
        if let Some(id) = redeemed {
            tx.send(AgentEvent::ApprovalRedeemed { id }).await.ok();
        }
        if let Some(id) = cancelled {
            tx.send(AgentEvent::ApprovalCancelled { id }).await.ok();
        }

        tx.send(AgentEvent::ToolResult(result.clone())).await.ok();
        turn_messages.push(ChatMessage::tool_result(result, call.id));
    }

    ToolExecResult::Messages(turn_messages)
}

// ---------------------------------------------------------------------------
// Within-tier failover advance
// ---------------------------------------------------------------------------

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
async fn advance_failover(
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use just_llm_client::LlmBackend;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use crate::agent_task::RoundToken;
    use crate::profile::BackendSource;
    use crate::test_support::{MapSource, ctx_from_source, make_ctx, profile};

    fn no_cancel() -> CancellationToken {
        CancellationToken::new()
    }

    // --- advance_failover unit tests (fast, no wiremock; summarize_and_evict no-ops on empty store) ---

    #[tokio::test]
    async fn advance_lands_on_next_buildable() {
        let profiles = vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 500_000)];
        let mut ctx = make_ctx(profiles, &["ep1", "ep2"]).await;
        let before_window = ctx.config.context_window_tokens;

        let outcome =
            advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;

        match outcome {
            FailoverOutcome::Advanced { from, to, messages } => {
                assert_eq!(from, "p1");
                assert_eq!(to, "p2");
                assert_eq!(ctx.failover.profile_idx(), 1);
                assert_eq!(ctx.client.model(), "p2-model");
                assert!(
                    messages.is_empty(),
                    "no compaction → prior (empty) messages returned"
                );
            }
            other => panic!("expected Advanced, got {other:?}"),
        }
        // p2 carries the same window as the config default → unchanged after advance.
        assert_eq!(ctx.config.context_window_tokens, before_window);
    }

    #[tokio::test]
    async fn advance_chain_exhausted_single_profile() {
        let mut ctx = make_ctx(vec![profile("p1", "ep1", 500_000)], &["ep1"]).await;
        let outcome =
            advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;
        match outcome {
            FailoverOutcome::ChainExhausted {
                reason: FailoverChainExhaustion::NoFailoverConfigured,
                ..
            } => {}
            other => panic!("expected ChainExhausted(NoFailoverConfigured), got {other:?}"),
        }
        assert_eq!(
            ctx.failover.profile_idx(),
            0,
            "index must not advance on exhaustion"
        );
    }

    #[tokio::test]
    async fn advance_cancelled_when_round_cancelled() {
        let mut ctx = make_ctx(
            vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 500_000)],
            &["ep1", "ep2"],
        )
        .await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &cancel).await;
        assert!(matches!(outcome, FailoverOutcome::Cancelled));
        assert_eq!(
            ctx.failover.profile_idx(),
            0,
            "index must not advance on cancel"
        );
    }

    #[tokio::test]
    async fn advance_skips_unbuildable_lands_on_next() {
        // [p1(ep1), p2(ep2-missing), p3(ep3)] → skip p2, land on p3.
        let profiles = vec![
            profile("p1", "ep1", 500_000),
            profile("p2", "ep2", 500_000),
            profile("p3", "ep3", 500_000),
        ];
        let mut ctx = make_ctx(profiles, &["ep1", "ep3"]).await;
        let outcome =
            advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;
        match outcome {
            FailoverOutcome::Advanced { from, to, .. } => {
                assert_eq!(from, "p1");
                assert_eq!(to, "p3", "p2 is unbuildable and must be skipped");
                assert_eq!(ctx.failover.profile_idx(), 2);
                assert_eq!(ctx.client.model(), "p3-model");
            }
            other => panic!("expected Advanced, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn advance_all_unbuildable_chain_exhausted() {
        // [p1(ep1), p2(ep2-missing)] → p2 unbuildable, no further candidate → ChainExhausted,
        // index unchanged, client still p1's (never left on an unbuilt profile).
        let mut ctx = make_ctx(
            vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 500_000)],
            &["ep1"],
        )
        .await;
        let outcome =
            advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;
        assert!(
            matches!(
                outcome,
                FailoverOutcome::ChainExhausted {
                    reason: FailoverChainExhaustion::AllCandidatesUnbuildable,
                    ..
                }
            ),
            "expected ChainExhausted(AllCandidatesUnbuildable), got {outcome:?}"
        );
        assert_eq!(ctx.failover.profile_idx(), 0, "index must not advance");
        assert_eq!(
            ctx.client.model(),
            "p1-model",
            "client must remain the active profile's"
        );
    }

    #[tokio::test]
    async fn advance_reapplies_smaller_window() {
        // p2 declares a smaller but valid window → config window tracks it after advance.
        let profiles = vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 100_000)];
        let mut ctx = make_ctx(profiles, &["ep1", "ep2"]).await;
        let outcome =
            advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;
        assert!(matches!(outcome, FailoverOutcome::Advanced { .. }));
        assert_eq!(ctx.config.context_window_tokens, 100_000);
        assert_eq!(ctx.failover.profile_idx(), 1);
    }

    #[tokio::test]
    async fn advance_skips_window_that_violates_invariant() {
        // [p1(500k), p2(10k infeasible), p3(500k)] all buildable → p2's window violates the budget
        // shape (summary_max > pinned at 10k) so it is skipped pre-advance; failover lands on p3.
        let profiles = vec![
            profile("p1", "ep1", 500_000),
            profile("p2", "ep2", 10_000),
            profile("p3", "ep3", 500_000),
        ];
        let mut ctx = make_ctx(profiles, &["ep1", "ep2", "ep3"]).await;

        let outcome =
            advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;

        match outcome {
            FailoverOutcome::Advanced { from, to, messages } => {
                assert_eq!(from, "p1");
                assert_eq!(to, "p3", "infeasible p2 is skipped, lands on p3");
                assert_eq!(ctx.failover.profile_idx(), 2);
                assert_eq!(ctx.client.model(), "p3-model");
                assert!(messages.is_empty());
            }
            other => panic!("expected Advanced (p2 skipped → p3), got {other:?}"),
        }
        assert_ne!(
            ctx.config.context_window_tokens, 10_000,
            "p2's infeasible window must never be adopted"
        );
    }

    #[tokio::test]
    async fn advance_all_infeasible_chain_exhausted() {
        // [p1(500k), p2(10k infeasible)] → p2 builds but its window violates the budget shape, is
        // skipped pre-advance, and no candidate remains → ChainExhausted(AllCandidatesInfeasible).
        // Index unchanged, client stays p1's (never swapped to an infeasible-window profile).
        let mut ctx = make_ctx(
            vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 10_000)],
            &["ep1", "ep2"],
        )
        .await;
        let before = ctx.config.context_window_tokens;
        let outcome =
            advance_failover(&mut ctx, vec![], anyhow::anyhow!("trigger"), &no_cancel()).await;
        assert!(
            matches!(
                outcome,
                FailoverOutcome::ChainExhausted {
                    reason: FailoverChainExhaustion::AllCandidatesInfeasible,
                    ..
                }
            ),
            "expected ChainExhausted(AllCandidatesInfeasible), got {outcome:?}"
        );
        assert_eq!(ctx.failover.profile_idx(), 0, "index must not advance");
        assert_eq!(
            ctx.client.model(),
            "p1-model",
            "client stays on the active profile (never swapped to p2)"
        );
        assert_eq!(
            ctx.config.context_window_tokens, before,
            "infeasible window never adopted"
        );
    }

    // --- run_agent_rounds integration tests (wiremock, one MockServer per profile) ---

    /// Fast retry policy so the wiremock suite stays snappy.
    fn fast_policy() -> crate::retry::RetryPolicy {
        crate::retry::RetryPolicy {
            max_retries: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            retry_timeout: Duration::from_secs(10),
        }
    }

    /// A real OpenAI-compatible backend pointed at `uri` (a wiremock server).
    fn wiremock_backend(uri: &str) -> Arc<dyn LlmBackend> {
        just_llm_client::provider::OpenAiCompatBackend::new(
            reqwest::Client::builder().use_rustls_tls(),
            "test-key",
            Some(uri),
        )
        .expect("openai-compat backend constructs without network")
    }

    async fn mount_status(server: &MockServer, status: u16) {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(status))
            .mount(server)
            .await;
    }

    /// Mount a 200 streaming response carrying `content` (no tool calls → `Finished`).
    async fn mount_ok_stream(server: &MockServer, content: &str) {
        let body = format!(
            "data: {{\"id\":\"s\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\ndata: [DONE]\n"
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body.into_bytes(), "text/event-stream"),
            )
            .mount(server)
            .await;
    }

    /// A `MapSource` mapping endpoint id → wiremock backend.
    fn wiremock_source(map: HashMap<String, Arc<dyn LlmBackend>>) -> Arc<dyn BackendSource> {
        Arc::new(MapSource(map))
    }

    /// Drive one `run_agent_rounds`: seed a user turn, mint a round token, run, collect events.
    async fn run_rounds(ctx: &mut AgentContext) -> (Result<AgentOutcome>, Vec<AgentEvent>) {
        ctx.record_turn(vec![ChatMessage::user(
            "respond with the single word: done",
        )])
        .await;
        let round = RoundToken::new(&ctx.cancel);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
        let (_prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel::<String>(16);
        let outcome = run_agent_rounds(ctx, &tx, &mut prompt_rx, round.handle()).await;
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        (outcome, events)
    }

    fn failover_hops(events: &[AgentEvent]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Failover { from, to, .. } => Some((from.clone(), to.clone())),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn failover_primary_down_backup_succeeds() {
        let primary = MockServer::start().await;
        let backup = MockServer::start().await;
        mount_status(&primary, 500).await; // exhausts retries → Failover
        mount_ok_stream(&backup, "done").await; // 200 → Finished

        let mut map = HashMap::new();
        map.insert("ep1".into(), wiremock_backend(&primary.uri()));
        map.insert("ep2".into(), wiremock_backend(&backup.uri()));
        let profiles = vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 500_000)];
        let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

        let (outcome, events) = run_rounds(&mut ctx).await;

        let content = match outcome {
            Ok(AgentOutcome::Finished { content }) => content,
            _ => panic!("expected Finished"),
        };
        assert_eq!(content, "done");
        assert_eq!(
            ctx.failover.profile_idx(),
            1,
            "should have failed over to backup"
        );
        assert_eq!(
            failover_hops(&events),
            vec![("p1".to_string(), "p2".to_string())],
            "exactly one failover p1→p2"
        );
    }

    #[tokio::test]
    async fn fatal_400_no_failover() {
        let primary = MockServer::start().await;
        mount_status(&primary, 400).await; // request-level → Fatal, no failover

        let mut map = HashMap::new();
        map.insert("ep1".into(), wiremock_backend(&primary.uri()));
        let profiles = vec![profile("p1", "ep1", 500_000), profile("p2", "ep2", 500_000)];
        let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

        let (outcome, events) = run_rounds(&mut ctx).await;

        assert!(outcome.is_err(), "400 is Fatal → round errors");
        assert_eq!(ctx.failover.profile_idx(), 0);
        assert!(
            failover_hops(&events).is_empty(),
            "no failover event on a Fatal"
        );
    }

    #[tokio::test]
    async fn chain_exhausted_single_profile_500() {
        let primary = MockServer::start().await;
        mount_status(&primary, 500).await; // exhausts → Failover, but no candidate → ChainExhausted

        let mut map = HashMap::new();
        map.insert("ep1".into(), wiremock_backend(&primary.uri()));
        let profiles = vec![profile("p1", "ep1", 500_000)]; // single-profile tier
        let mut ctx = ctx_from_source(profiles, wiremock_source(map), fast_policy()).await;

        let (outcome, events) = run_rounds(&mut ctx).await;

        match outcome {
            Ok(AgentOutcome::FailoverChainExhausted {
                reason: FailoverChainExhaustion::NoFailoverConfigured,
                ..
            }) => {}
            other => panic!("expected FailoverChainExhausted(NoFailoverConfigured), got {other:?}"),
        }
        assert_eq!(ctx.failover.profile_idx(), 0);
        assert!(
            failover_hops(&events).is_empty(),
            "no failover event when the chain is exhausted"
        );
    }
}
