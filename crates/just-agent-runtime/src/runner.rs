//! Agent round execution loop.

use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::agent_task::AgentContext;
use crate::approval::format_approval_notifications;
use crate::context::{AgenticContext, compose_context};
use crate::event::{AgentEvent, AgentOutcome};
use crate::failover::FailoverOutcome;
use just_llm_client::types::chat::{
    ChatMessage, ChatToolCall, StreamOptions, ToolCallsMessage, ToolChoice, ToolChoiceMode,
    ToolDefinition,
};

use crate::stream_accumulator::ToolCallAccumulator;
use just_agent_common::protocol::FailoverChainExhaustion;
use just_agent_common::tokens::format_tokens_m;

/// Outcome of context compaction via [`summarize_and_evict`].
pub(crate) enum CompactOutcome {
    /// Some turns were summarized and evicted.
    Compacted,
    /// No turns to compact (context already within budget).
    NothingToCompact,
    /// Token budget exceeded during summarization.
    BudgetExceeded { consumed: u64, budget: u64 },
}

// ---------------------------------------------------------------------------
// Stream consumption types
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
// Agent round loop
// ---------------------------------------------------------------------------

/// Run the agent round loop until completion or max rounds.
pub async fn run_agent_rounds(
    ctx: &mut AgentContext,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
    round_cancel: &CancellationToken,
) -> Result<AgentOutcome> {
    let tool_timeout = Duration::from_secs(ctx.config.tool_timeout_secs);

    for _round in 0..ctx.config.max_tool_rounds {
        // -- Interjection draining: consume queued prompts/commands --
        {
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

        // -- Context composition and LLM request --
        let mut messages = compose_context(ctx.store.clone()).await;
        let tools = ctx.store.lock().await.tool_definitions().to_vec();

        // -- Token budget check --
        // Incremental when possible (anchored to the last response's authoritative
        // `last_prompt_tokens` + the delta since), full otherwise. No throwaway request is built
        // here: the estimator takes the composed messages/tools/system prompt directly. The SEND
        // request is built per profile inside the failover loop below.
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

        if prompt_tokens > 0 {
            let effective_budget = ctx.config.effective_budget();
            let usage_pct = prompt_tokens * 100 / effective_budget;

            // Phase 1: Progressive warnings.
            if check_progressive_warnings(ctx, usage_pct, effective_budget).await {
                continue;
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
                    Ok(CompactOutcome::Compacted) => continue,
                    Ok(CompactOutcome::NothingToCompact) => {} // fall through
                    Ok(CompactOutcome::BudgetExceeded { consumed, budget }) => {
                        return Ok(AgentOutcome::TokenBudgetExceeded { consumed, budget });
                    }
                    Err(e) => warn!("summarize_and_evict failed: {e:#}"),
                }
                if round_cancel.is_cancelled() {
                    return Ok(AgentOutcome::Cancelled);
                }
            }
        }

        // -- Within-tier failover loop --
        // The request is rebuilt per profile (each profile has its own model + endpoint). On a
        // `Failover` outcome (endpoint-level failure, or transient retries exhausted) the runner
        // advances to the next profile in the tier, rebuilds the client, and retries the same
        // turn. On `Fatal` (request-level) it errors the round. `profile_idx` only moves forward
        // and sticks for the agent's lifetime (resets to 0 on spawn/restore).
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
                        round: _round,
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
                        return Ok(AgentOutcome::Cancelled);
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
                    return Err(e.into());
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
                            return Ok(AgentOutcome::FailoverChainExhausted {
                                reason,
                                detail: format!("{trigger:#}"),
                            });
                        }
                        FailoverOutcome::Cancelled => return Ok(AgentOutcome::Cancelled),
                        FailoverOutcome::BudgetExceeded { consumed, budget } => {
                            return Ok(AgentOutcome::TokenBudgetExceeded { consumed, budget });
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
                    return Ok(AgentOutcome::Cancelled);
                }
            }
        };
        if !retry_records.is_empty() {
            ctx.store.lock().await.retry_log.extend(retry_records);
            ctx.persist().await;
        }

        // -- Stream consumption --
        let consumed = match consume_stream(stream, tx, round_cancel).await {
            StreamOutcome::Cancelled => return Ok(AgentOutcome::Cancelled),
            StreamOutcome::Completed(c) => c,
        };

        // -- Post-stream: usage accumulation and token budget check --
        if let Some(usage) = consumed.usage {
            ctx.store.lock().await.accumulate_usage(&usage);
            ctx.token_budget
                .record_usage(usage.prompt_tokens as u64, usage.completion_tokens as u64);
        }

        // Reload budget — the operator may have increased it via API mid-round.
        let snap = ctx.token_budget.snapshot();

        // Token budget warning injection (before exhaustion check).
        if check_token_budget_warnings(ctx, &snap).await {
            continue;
        }

        // Token budget exhaustion check (shared tree-wide counter).
        if snap.is_exceeded() {
            return Ok(AgentOutcome::TokenBudgetExceeded {
                consumed: snap.consumed,
                budget: snap.budget,
            });
        }

        if consumed.tool_calls.is_empty() {
            if !consumed.content.is_empty() {
                return Ok(AgentOutcome::Finished {
                    content: consumed.content,
                });
            }
            bail!("assistant returned neither tool calls nor final content");
        }

        // -- Tool execution loop --
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
                        return Ok(AgentOutcome::Cancelled);
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

        ctx.record_turn(turn_messages).await;
        ctx.persist().await;
    }

    Ok(AgentOutcome::MaxRoundsExceeded)
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

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Estimate the prompt-token size of the next request.
///
/// **Incremental** (normal round, no prefix change since the last response): the authoritative
/// `last_prompt_tokens` from the last provider response — which already counts system prompt +
/// tools + pinned + turns[0..anchor] — plus a chars/4 render-estimate of *only* the turns added
/// since that response (the assistant turn just completed, its tool results, and the next user
/// prompt). Exact base + a small approximated delta: cheaper than a full render and more accurate
/// than re-estimating the whole history.
///
/// **Full** (first round ever, after any prefix-mutating op, after failover, or after restore):
/// a chars/4 render of `system_prompt + messages + tools`. Required whenever the persisted anchor
/// can't be trusted — e.g. a restore following an agent-version upgrade may have changed the
/// system prompt or tool set (see `ContextStore::needs_full_estimate`). `messages` is the
/// `compose_context` output (`pinned ++ turns`, no system prompt); the system prompt is rendered
/// separately so the full estimate matches what the provider receives.
async fn estimate_context_tokens(
    client: &crate::profile::ChatClient,
    store: &tokio::sync::Mutex<crate::context::ContextStore>,
    messages: &[ChatMessage],
    tools: &[ToolDefinition],
    system_prompt: Option<&str>,
) -> Result<usize> {
    let g = store.lock().await;
    match (g.last_prompt_tokens(), g.needs_full_estimate()) {
        (Some(base), false) => {
            // Incremental: only the turns added since the anchor.
            let turns_len = g.turns().len();
            let anchored = g.anchored_turn_count();
            debug_assert!(
                anchored <= turns_len,
                "anchor out of range — a needs_full_estimate flag-set was missed"
            );
            if anchored > turns_len {
                warn!(
                    anchored,
                    turns_len, "estimate anchor clamped to turns length"
                );
            }
            let delta: Vec<ChatMessage> = g
                .turns()
                .iter()
                .skip(anchored.min(turns_len))
                .flat_map(|t| t.messages.iter().cloned())
                .collect();
            drop(g);
            Ok(base as usize + client.render_messages(&delta)?.chars().count() / 4)
        }
        _ => {
            // Full: system + messages + tools (the historical behavior).
            drop(g);
            let mut rendered = String::new();
            if let Some(sp) = system_prompt {
                rendered.push_str(&client.render_messages(&[ChatMessage::system(sp)])?);
            }
            rendered.push_str(&client.render_messages(messages)?);
            rendered.push_str(&client.render_tools(tools)?);
            Ok(rendered.chars().count() / 4)
        }
    }
}

// ---------------------------------------------------------------------------
// Context compaction
// ---------------------------------------------------------------------------

/// Summarize turns to bring context within budget.
///
/// Loops in bounded passes: each pass summarizes the oldest turns that fit
/// in the summarizer input budget, accumulates into the existing summary,
/// and evicts the summarized turns. Repeats until context fits or no
/// progress can be made.
pub(crate) async fn summarize_and_evict(ctx: &AgentContext) -> Result<CompactOutcome> {
    let effective_budget = ctx.config.effective_budget();
    let summarizer_input_budget =
        effective_budget.saturating_sub(ctx.summarizer.max_tokens as usize);
    let mut any_summarized = false;

    loop {
        // Read phase: snapshot under single lock.
        let (window, existing_summary) = {
            let guard = ctx.store.lock().await;
            if guard.turn_count() == 0 {
                break;
            }
            if guard.total_estimated_tokens() <= effective_budget {
                break;
            }
            let existing_summary = guard
                .pinned()
                .iter()
                .find(|p| p.label == "context_summary")
                .and_then(|p| p.message.content().map(|c| c.to_owned()));

            // Take oldest turns that fit in summarizer_input_budget.
            let mut budget = summarizer_input_budget;
            let mut window = Vec::new();
            for turn in guard.turns().iter() {
                if turn.estimated_tokens > budget {
                    break;
                }
                budget -= turn.estimated_tokens;
                window.push(turn.clone());
            }
            if window.is_empty() {
                break;
            }
            (window, existing_summary)
        };

        // LLM call — lock released during this potentially long await.
        let (result, usage) = ctx
            .summarizer
            .summarize(
                &window,
                existing_summary.as_deref(),
                effective_budget,
                &ctx.client,
            )
            .await?;

        // Write phase: replace summary + evict turns — single lock, no await.
        {
            let mut guard = ctx.store.lock().await;
            guard.replace_pin("context_summary", ChatMessage::assistant(&result.text))?;
            guard.evict_turns(result.source_turns);
            guard.reset_context_warnings();
            info!(
                source_turns = result.source_turns,
                estimated_tokens = result.estimated_tokens,
                "summarize pass completed"
            );
        }

        // Record compaction event in history.
        let summary_msg = vec![ChatMessage::assistant(&result.text)];
        ctx.append_history(
            None,
            &summary_msg,
            result.estimated_tokens,
            crate::history::RecordKind::System,
            Some(crate::history::SystemEvent::CompactionSummary),
        );

        // Accumulate usage from the summarization LLM call WITHOUT re-anchoring: the summarizer
        // runs over a different message set (oldest turns + SUMMARIZE_PROMPT), so its
        // `prompt_tokens` does not reflect the main conversation. Bumping `cumulative_usage`
        // (operator budget) is correct; moving the prompt anchor would poison the next estimate.
        if let Some(u) = &usage {
            ctx.store.lock().await.accumulate_usage_no_anchor(u);
            ctx.token_budget
                .record_usage(u.prompt_tokens as u64, u.completion_tokens as u64);
        }

        // Check token budget after accumulating summarization usage.
        let snap = ctx.token_budget.snapshot();
        if snap.is_exceeded() {
            ctx.persist().await;
            return Ok(CompactOutcome::BudgetExceeded {
                consumed: snap.consumed,
                budget: snap.budget,
            });
        }

        any_summarized = true;
    }

    if any_summarized {
        ctx.persist().await;
    }

    Ok(if any_summarized {
        CompactOutcome::Compacted
    } else {
        CompactOutcome::NothingToCompact
    })
}

/// Check progressive warning thresholds and inject a [system] message if crossed.
/// Returns `true` if a warning was injected (caller should continue to re-compose).
async fn check_progressive_warnings(
    ctx: &mut AgentContext,
    usage_pct: usize,
    effective_budget: usize,
) -> bool {
    let warnings = ctx.config.warning_thresholds();
    if warnings.is_empty() {
        return false;
    }

    let mut guard = ctx.store.lock().await;

    // If usage is below the lowest threshold, reset warning state.
    let lowest = warnings[0] as usize;
    if usage_pct < lowest {
        guard.reset_context_warnings();
        return false;
    }

    // Find the highest crossed threshold that hasn't been warned yet.
    let Some(threshold) = warnings
        .iter()
        .rev()
        .find(|&&t| usage_pct >= t as usize && guard.should_warn(t))
        .copied()
    else {
        return false;
    };

    let msg = format!(
        "[system]\nContext usage is at {}% ({} / {} tokens). \
         Use context_status to review current turns, then context_evict with a \
         summary to evict all turns while preserving key facts.",
        threshold,
        effective_budget * threshold as usize / 100,
        effective_budget
    );

    guard.mark_warned(threshold);
    let msgs = vec![ChatMessage::user(&msg)];
    let (turn_id, estimated_tokens) = guard.push_turn(msgs.clone());
    drop(guard);
    ctx.append_history(
        Some(turn_id.0),
        &msgs,
        estimated_tokens,
        crate::history::RecordKind::Turn,
        None,
    );
    info!(threshold, "injected context warning");
    true
}

/// Check token budget warning thresholds and inject a [system] message if crossed.
/// Returns `true` if a warning was injected (caller should continue to re-compose).
async fn check_token_budget_warnings(
    ctx: &mut AgentContext,
    snap: &crate::token_budget::TokenBudgetSnapshot,
) -> bool {
    let warnings = &ctx.config.token_budget_warnings;
    if warnings.is_empty() {
        return false;
    }

    let pct = snap.usage_pct();
    // usage_pct() returns 0 when budget is 0, so no warnings fire.
    if pct == 0 {
        return false;
    }

    let mut guard = ctx.store.lock().await;

    // Find the highest crossed threshold that hasn't been warned yet.
    let Some(threshold) = warnings
        .iter()
        .rev()
        .find(|&&t| pct >= t && guard.should_warn_budget(t))
        .copied()
    else {
        return false;
    };

    let msg = format!(
        "[system]\nToken budget usage is at {}% ({} consumed, {} remaining of {}). \
         Wrap up your current work concisely.",
        threshold,
        format_tokens_m(snap.consumed),
        format_tokens_m(snap.remaining()),
        format_tokens_m(snap.budget)
    );

    guard.mark_budget_warned(threshold);
    let msgs = vec![ChatMessage::user(&msg)];
    let (turn_id, estimated_tokens) = guard.push_turn(msgs.clone());
    drop(guard);
    ctx.append_history(
        Some(turn_id.0),
        &msgs,
        estimated_tokens,
        crate::history::RecordKind::Turn,
        None,
    );
    info!(
        threshold,
        consumed = snap.consumed,
        budget = snap.budget,
        "injected token budget warning"
    );
    true
}

/// Compact context if it exceeds the budget.
/// Called at agent startup for restored agents.
pub async fn compact_if_needed(ctx: &AgentContext) -> Result<bool> {
    let effective_budget = ctx.config.effective_budget();
    let total_tokens = {
        let guard = ctx.store.lock().await;
        guard.total_estimated_tokens()
    };

    if total_tokens <= effective_budget {
        return Ok(false);
    }

    info!(total_tokens, effective_budget, "pre-loop compaction needed");
    match summarize_and_evict(ctx).await? {
        CompactOutcome::Compacted => Ok(true),
        CompactOutcome::NothingToCompact => Ok(false),
        // Budget exceeded during pre-loop compaction — log and proceed.
        // The pre-call budget check at the top of the first round will catch it.
        CompactOutcome::BudgetExceeded { consumed, budget } => {
            warn!(
                consumed,
                budget, "token budget exceeded during pre-loop compaction"
            );
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    use anyhow::Context;
    use just_llm_client::{LlmBackend, ToolDispatcher};
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use crate::approval::ApprovalStore;
    use crate::config::{AgentConfig, PermissionProfile, default_tool_policy};
    use crate::context::{ContextStore, ContextSummarizer};
    use crate::failover::{FailoverOutcome, FailoverState};
    use crate::policy::{AgentPolicy, AuthorizedToolExecutor};
    use crate::profile::{BackendSource, Profile, ProfileRegistry, Tier};
    use crate::token_budget::TokenBudget;

    // --- fixtures (mirror profile/registry.rs tests; duplicated to keep this module self-contained) ---

    /// Network-free DeepSeek backend (construction touches no network).
    fn ds_backend() -> Arc<dyn just_llm_client::LlmBackend> {
        just_llm_client::provider::DeepSeekBackend::new(
            reqwest::Client::builder().use_rustls_tls(),
            "fake",
            None,
        )
        .expect("deepseek backend constructs without network")
    }

    /// Test [`BackendSource`]: endpoint id → backend. A missing endpoint yields `Err`, used to
    /// simulate an unbuildable failover candidate (the skip path).
    struct MapSource(HashMap<String, Arc<dyn just_llm_client::LlmBackend>>);
    impl BackendSource for MapSource {
        fn get(&self, endpoint_id: &str) -> anyhow::Result<Arc<dyn just_llm_client::LlmBackend>> {
            self.0
                .get(endpoint_id)
                .cloned()
                .with_context(|| format!("unknown endpoint '{endpoint_id}'"))
        }
    }

    fn profile(id: &str, endpoint: &str, window: usize) -> Profile {
        Profile {
            id: id.into(),
            endpoint: endpoint.into(),
            model: format!("{id}-model"),
            max_context_window: window,
        }
    }

    /// Minimal valid `AgentConfig` for failover tests (mirrors `config.rs` fixtures).
    fn test_config() -> AgentConfig {
        AgentConfig {
            prompt: None,
            system_prompt: String::new(),
            max_tool_rounds: 1,
            workspace_root: PathBuf::from("/tmp"),
            context_window_tokens: 500_000,
            output_reserve_tokens: 8_192,
            summary_max_tokens: 1_200,
            tool_timeout_secs: 120,
            skills: vec![],
            retry_policy: crate::retry::RetryPolicy::default(),
            pinned_budget_ratio: 0.25,
            context_thresholds: vec![50, 80],
            token_budget_warnings: vec![80, 95],
            agent_id: None,
            created_by: None,
            permissions: PermissionProfile::new(PathBuf::from("/tmp")),
        }
    }

    /// Build an `AgentContext` over `profiles` backed by `source`, with `retry_policy`. The store
    /// starts empty (seed a user turn for `run_agent_rounds` tests); `summarize_and_evict` no-ops
    /// on it. `profiles[0]` must be buildable (its client is constructed here).
    async fn ctx_from_source(
        profiles: Vec<Profile>,
        source: Arc<dyn BackendSource>,
        retry_policy: crate::retry::RetryPolicy,
    ) -> AgentContext {
        let mut config = test_config();
        config.retry_policy = retry_policy;
        let tier = Tier { profiles };
        let registry = Arc::new(ProfileRegistry::new(vec![tier.clone()], source).unwrap());
        let failover = FailoverState::new(tier, registry, Some("sys".into()));
        let client = failover
            .build_client(failover.current_profile())
            .expect("active profile is buildable");
        let store = Arc::new(tokio::sync::Mutex::new(ContextStore::new()));
        let approvals = Arc::new(tokio::sync::Mutex::new(ApprovalStore::new()));
        let executor = AuthorizedToolExecutor::new(
            ToolDispatcher::new(),
            AgentPolicy::new(Arc::new(RwLock::new(default_tool_policy()))),
            approvals.clone(),
        );
        {
            let mut guard = store.lock().await;
            guard.set_tool_definitions(executor.tool_definitions());
            guard.set_pinned_budget(config.pinned_budget());
        }
        AgentContext {
            client,
            failover,
            store,
            approvals,
            executor,
            summarizer: ContextSummarizer::new(config.summary_max_tokens),
            config,
            agent_dir: None,
            history: None,
            cancel: CancellationToken::new(),
            round_cancel: Arc::new(std::sync::Mutex::new(None)),
            notify: Arc::new(tokio::sync::Notify::new()),
            token_budget: TokenBudget::new(1_000_000, 0),
        }
    }

    /// A `MapSource` of network-free DeepSeek backends for `endpoints` (unit-test convenience).
    fn map_source(endpoints: &[&str]) -> Arc<dyn BackendSource> {
        let mut map = HashMap::new();
        for ep in endpoints {
            map.insert((*ep).into(), ds_backend());
        }
        Arc::new(MapSource(map))
    }

    async fn make_ctx(profiles: Vec<Profile>, endpoints: &[&str]) -> AgentContext {
        ctx_from_source(
            profiles,
            map_source(endpoints),
            crate::retry::RetryPolicy::default(),
        )
        .await
    }

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

    // --- estimate_context_tokens (incremental, anchored to provider usage) ---

    fn usage(prompt_tokens: u32) -> just_llm_client::types::chat::Usage {
        just_llm_client::types::chat::Usage {
            prompt_tokens,
            completion_tokens: 0,
            prompt_cache_hit_tokens: None,
            prompt_cache_miss_tokens: None,
            total_tokens: prompt_tokens,
            completion_tokens_details: None,
        }
    }

    /// With an anchor and no turns added since, the incremental estimate equals the authoritative
    /// base exactly (empty delta → +0). Pins the incremental path and the anchor mechanic.
    #[tokio::test]
    async fn incremental_estimate_equals_base_with_no_new_turns() {
        let ctx = make_ctx(vec![profile("p1", "ep1", 500_000)], &["ep1"]).await;
        {
            let mut s = ctx.store.lock().await;
            s.push_turn(vec![ChatMessage::user("first turn")]);
            s.push_turn(vec![ChatMessage::user("second turn")]);
            s.accumulate_usage(&usage(5_000));
        }
        let est = estimate_context_tokens(&ctx.client, &ctx.store, &[], &[], None)
            .await
            .unwrap();
        assert_eq!(est, 5_000, "anchored + empty delta → base exactly");
    }

    /// A turn added after the anchor is reflected as a positive delta on top of the base.
    #[tokio::test]
    async fn incremental_estimate_grows_with_new_turn() {
        let ctx = make_ctx(vec![profile("p1", "ep1", 500_000)], &["ep1"]).await;
        {
            let mut s = ctx.store.lock().await;
            s.push_turn(vec![ChatMessage::user("first")]);
            s.accumulate_usage(&usage(5_000));
        }
        let before = estimate_context_tokens(&ctx.client, &ctx.store, &[], &[], None)
            .await
            .unwrap();
        ctx.store.lock().await.push_turn(vec![ChatMessage::user(
            "a brand new turn with some content",
        )]);
        let after = estimate_context_tokens(&ctx.client, &ctx.store, &[], &[], None)
            .await
            .unwrap();
        assert!(
            after > before,
            "delta of the new turn is added: {after} > {before}"
        );
    }

    /// A prefix-mutating op (evict) forces full mode: the estimate drops from the anchored base
    /// to a fresh full render, proving the flag flips the path off the (stale) anchor.
    #[tokio::test]
    async fn evict_forces_full_estimate_off_the_anchor() {
        let ctx = make_ctx(vec![profile("p1", "ep1", 500_000)], &["ep1"]).await;
        {
            let mut s = ctx.store.lock().await;
            s.push_turn(vec![ChatMessage::user("turn one")]);
            s.push_turn(vec![ChatMessage::user("turn two")]);
            // A huge authoritative base; the incremental path would report ~the base.
            s.accumulate_usage(&usage(5_000_000));
        }
        let incremental = estimate_context_tokens(&ctx.client, &ctx.store, &[], &[], None)
            .await
            .unwrap();
        assert!(
            incremental >= 5_000_000,
            "incremental path used before evict, got {incremental}"
        );

        // Evict invalidates the anchor → full mode recomputes a fresh render.
        ctx.store.lock().await.evict_turns(1);
        let full = estimate_context_tokens(&ctx.client, &ctx.store, &[], &[], None)
            .await
            .unwrap();
        assert!(
            full < 5_000_000,
            "evict forces full mode (fresh render), got {full}"
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
        let round = crate::agent_task::RoundToken::new(&ctx.cancel);
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
