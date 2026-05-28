//! Agent round execution loop.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use tracing::{info, warn};

use crate::context::{AgenticContext, compose_context};
use crate::deferred::DeferredNotification;
use crate::session::AgentContext;
use crate::types::{AgentEvent, AgentOutcome};
use just_llm_client::types::chat::{
    ChatCompletionChunkToolCall, ChatMessage, ChatToolCall, FunctionCall, StreamOptions,
    ToolCallsMessage, ToolChoice, ToolChoiceMode, ToolType,
};

/// Run the agent round loop until completion or max rounds.
pub async fn run_agent_rounds(
    ctx: &mut AgentContext,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) -> Result<AgentOutcome> {
    let tool_timeout = Duration::from_secs(ctx.config.tool_timeout_secs);
    let context_window = ctx.config.context_window_tokens;

    for _round in 0..ctx.config.max_tool_rounds {
        // Inject deferred approval notifications into context.
        let notifications = ctx.deferred.lock().await.drain_notifications();
        if !notifications.is_empty() {
            let msg = format_deferred_notifications(&notifications);
            ctx.store
                .lock()
                .await
                .push_turn(vec![ChatMessage::user(&msg)]);
        }

        let messages = compose_context(ctx.store.clone()).await;

        let mut request = ctx
            .client
            .request(messages)
            .with_tools(ctx.store.lock().await.tool_definitions().to_vec())
            .with_tool_choice(ToolChoice::Mode(ToolChoiceMode::Auto));

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

        // Two-phase threshold check: progressive warnings, then auto-compact.
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
                    Ok(true) => continue,
                    Ok(false) => {} // nothing to compact, fall through
                    Err(e) => warn!("summarize_and_evict failed: {e:#}"),
                }
                if ctx.cancel.is_cancelled() {
                    return Ok(AgentOutcome::Cancelled);
                }
            }
        }

        // Enable streaming
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

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tool_acc = ToolCallAccumulator::new();
        let mut usage_prompt_tokens: Option<u32> = None;

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

                    if let Some(usage) = &chunk.usage {
                        usage_prompt_tokens = Some(usage.prompt_tokens);
                    }
                }
                _ = ctx.cancel.cancelled() => {
                    tracing::info!("LLM stream cancelled mid-stream");
                    return Ok(AgentOutcome::Cancelled);
                }
            }
        }

        if let Some(pt) = usage_prompt_tokens {
            ctx.store.lock().await.set_last_usage(pt);
        }

        let tool_calls = tool_acc.finish();
        if tool_calls.is_empty() {
            if !content.is_empty() {
                return Ok(AgentOutcome::Finished { content });
            }
            bail!("assistant returned neither tool calls nor final content");
        }

        let mut turn_messages = vec![ChatMessage::ToolCalls(ToolCallsMessage {
            role: "assistant".into(),
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            name: None,
            tool_calls: tool_calls.clone(),
            reasoning_content: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
        })];

        for call in tool_calls {
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

            // Check if this was a deferred action and emit DeferredCreated.
            if let Some(info) = ctx.deferred.lock().await.take_last_deferred() {
                tx.send(AgentEvent::DeferredCreated {
                    request_id: info.request_id,
                    tool_name: info.tool_name,
                    summary: info.summary,
                    reason: info.reason,
                    dangerous: info.dangerous,
                })
                .await
                .ok();
            }

            tx.send(AgentEvent::ToolResult(result.clone())).await.ok();
            turn_messages.push(ChatMessage::tool_result(result, call.id));
        }

        ctx.store.lock().await.push_turn(turn_messages);
        ctx.persist().await;
    }

    Ok(AgentOutcome::MaxRoundsExceeded)
}

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

/// Summarize turns to bring context within budget.
///
/// Loops in bounded passes: each pass summarizes the oldest turns that fit
/// in the summarizer input budget, accumulates into the existing summary,
/// and evicts the summarized turns. Repeats until context fits or no
/// progress can be made.
///
/// Returns `Ok(true)` if any summarization was performed.
pub(crate) async fn summarize_and_evict(ctx: &AgentContext) -> Result<bool> {
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
        let result = ctx
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
            guard.reset_warnings();
            info!(
                source_turns = result.source_turns,
                estimated_tokens = result.estimated_tokens,
                "summarize pass completed"
            );
        }

        any_summarized = true;
    }

    if any_summarized {
        ctx.persist().await;
    }

    Ok(any_summarized)
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
        guard.reset_warnings();
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
    guard.push_turn(vec![ChatMessage::user(&msg)]);
    drop(guard);
    info!(threshold, "injected context warning");
    true
}

/// Compact context if it exceeds the budget.
/// Called at agent startup for restored sessions.
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
    summarize_and_evict(ctx).await
}

fn format_deferred_notifications(notifications: &[DeferredNotification]) -> String {
    let mut parts = Vec::new();
    for n in notifications {
        match n {
            DeferredNotification::Approved {
                request_id,
                summary,
            } => {
                parts.push(format!(
                    "Deferred action {request_id} (\"{summary}\") has been approved. \
                     Call approval_redeem with this request_id to execute."
                ));
            }
            DeferredNotification::Denied {
                request_id,
                summary,
                reason,
            } => {
                parts.push(format!(
                    "Deferred action {request_id} (\"{summary}\") has been denied: {reason}"
                ));
            }
        }
    }
    format!("[system]\n{}", parts.join("\n"))
}

// ---------------------------------------------------------------------------
// Streaming tool-call accumulator
// ---------------------------------------------------------------------------

struct AccumulatedToolCall {
    id: Option<String>,
    kind: Option<ToolType>,
    name: Option<String>,
    arguments: String,
}

struct ToolCallAccumulator {
    calls: BTreeMap<u32, AccumulatedToolCall>,
}

impl ToolCallAccumulator {
    fn new() -> Self {
        Self {
            calls: BTreeMap::new(),
        }
    }

    fn push(&mut self, delta: &ChatCompletionChunkToolCall) {
        let index = delta.index.unwrap_or(0);
        let entry = self.calls.entry(index).or_insert(AccumulatedToolCall {
            id: None,
            kind: None,
            name: None,
            arguments: String::new(),
        });
        if let Some(id) = &delta.id {
            entry.id = Some(id.clone());
        }
        if let Some(kind) = &delta.kind {
            entry.kind = Some(kind.clone());
        }
        if let Some(func) = &delta.function {
            if let Some(name) = &func.name {
                entry.name = Some(name.clone());
            }
            if let Some(args) = &func.arguments {
                entry.arguments.push_str(args);
            }
        }
    }

    fn finish(self) -> Vec<ChatToolCall> {
        self.calls
            .into_values()
            .map(|acc| ChatToolCall {
                id: acc.id.unwrap_or_default(),
                kind: acc.kind.unwrap_or(ToolType::Function),
                function: FunctionCall {
                    name: acc.name.unwrap_or_default(),
                    arguments: acc.arguments,
                },
            })
            .collect()
    }
}
