//! Agent round execution loop.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::agent_task::AgentContext;
use crate::approval::format_approval_notifications;
use crate::context::{AgenticContext, compose_context};
use crate::event::{AgentEvent, AgentOutcome};
use just_llm_client::types::chat::{
    ChatMessage, ChatToolCall, StreamOptions, ToolCallsMessage, ToolChoice, ToolChoiceMode,
};

use crate::stream_accumulator::ToolCallAccumulator;
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
) -> Result<AgentOutcome> {
    let tool_timeout = Duration::from_secs(ctx.config.tool_timeout_secs);
    let context_window = ctx.config.context_window_tokens;

    for _round in 0..ctx.config.max_tool_rounds {
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
        let messages = compose_context(ctx.store.clone()).await;

        let mut request = ctx
            .client
            .request(messages)
            .with_tools(ctx.store.lock().await.tool_definitions().to_vec())
            .with_tool_choice(ToolChoice::Mode(ToolChoiceMode::Auto));

        // -- Token budget check --
        let prompt_tokens = {
            let estimate = estimate_prompt_tokens(&ctx.client, &request);
            tokio::select! {
                result = estimate => match result {
                    Ok(tokens) => tokens,
                    Err(e) => {
                        warn!("token estimation failed, sending request anyway: {e:#}");
                        0
                    }
                },
                _ = ctx.cancel.cancelled() => return Ok(AgentOutcome::Cancelled),
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
                info!(prompt_tokens, context_window, "context exceeds budget");
                match summarize_and_evict(ctx).await {
                    Ok(CompactOutcome::Compacted) => continue,
                    Ok(CompactOutcome::NothingToCompact) => {} // fall through
                    Ok(CompactOutcome::BudgetExceeded { consumed, budget }) => {
                        return Ok(AgentOutcome::TokenBudgetExceeded { consumed, budget });
                    }
                    Err(e) => warn!("summarize_and_evict failed: {e:#}"),
                }
                if ctx.cancel.is_cancelled() {
                    return Ok(AgentOutcome::Cancelled);
                }
            }
        }

        // -- Stream request with retry --
        request.stream = Some(true);
        request.stream_options = Some(StreamOptions {
            include_usage: Some(true),
        });

        let mut retry_records = Vec::new();
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
                .filter(|r| r.timestamp + window_secs > now)
                .count() as u32
        };
        let stream_result = {
            let stream_fut = crate::retry::stream_with_retry(
                &ctx.client,
                request,
                &ctx.config.retry_policy,
                _round,
                tx,
                &mut retry_records,
                prior_retries,
                ctx.cancel.clone(),
            );
            tokio::select! {
                result = stream_fut => result,
                _ = ctx.cancel.cancelled() => {
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
        let stream = stream_result?;

        // -- Stream consumption --
        let consumed = match consume_stream(stream, tx, &ctx.cancel).await {
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
                    _ = ctx.cancel.cancelled() => {
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
// Token estimation
// ---------------------------------------------------------------------------

/// Estimate prompt tokens via the ChatClient pipeline.
async fn estimate_prompt_tokens(
    client: &just_llm_client::ChatClient,
    request: &just_llm_client::types::chat::ChatCompletionRequest,
) -> Result<usize> {
    let estimator = client
        .token_estimation()
        .context("backend does not support token estimation")?;
    let prepared = client.prepared_request(request.clone()).await?;
    let estimate = estimator.estimate_tokens(&prepared).await?;
    Ok(estimate.prompt_tokens as usize)
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

        // Accumulate exact usage from the summarization LLM call.
        if let Some(u) = &usage {
            ctx.store.lock().await.accumulate_usage(u);
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
