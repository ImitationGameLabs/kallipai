//! Agent round execution loop and context compaction.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use tracing::{info, warn};

use crate::context::compose_context;
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
    let output_reserve = ctx.config.output_reserve_tokens;
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

        let prompt_tokens = match estimate_prompt_tokens(&ctx.client, &request).await {
            Ok(tokens) => tokens,
            Err(e) => {
                warn!("token estimation failed, sending request anyway: {e:#}");
                0
            }
        };

        if prompt_tokens > 0 && prompt_tokens + output_reserve > context_window {
            info!(
                prompt_tokens,
                context_window, "context exceeds budget, triggering compaction"
            );
            match compact_context(ctx).await {
                Ok(true) => continue,
                Ok(false) => {} // nothing to compact, fall through
                Err(e) => warn!("compaction failed: {e:#}"),
            }
        }

        // Enable streaming
        request.stream = Some(true);
        request.stream_options = Some(StreamOptions { include_usage: Some(true) });

        let stream = ctx.client.stream_chat_completion(request).await?;

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tool_acc = ToolCallAccumulator::new();
        let mut usage_prompt_tokens: Option<u32> = None;

        tokio::pin!(stream);
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
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
            content: if content.is_empty() { None } else { Some(content) },
            name: None,
            tool_calls: tool_calls.clone(),
            reasoning_content: if reasoning.is_empty() { None } else { Some(reasoning) },
        })];

        for call in tool_calls {
            tx.send(AgentEvent::ToolCall {
                name: call.function.name.clone(),
                args: call.function.arguments.clone(),
            })
            .await
            .ok();
            let result = match tokio::time::timeout(
                tool_timeout,
                ctx.executor
                    .execute(&call.function.name, &call.function.arguments),
            )
            .await
            {
                Ok(output) => output,
                Err(_) => format!(
                    "tool '{}' timed out after {}s",
                    call.function.name,
                    tool_timeout.as_secs()
                ),
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

/// Drain old turns, run compaction strategy, write back results.
///
/// Returns `Ok(true)` if compaction was performed, `Ok(false)` if
/// there were no turns to compact.
pub async fn compact_context(ctx: &AgentContext) -> Result<bool> {
    let (drained, existing_summary) = {
        let mut guard = ctx.store.lock().await;
        let turn_count = guard.turn_count();
        if turn_count == 0 {
            return Ok(false);
        }
        let drained = guard.drain_turns(0..turn_count);
        let summary = guard.summary().map(|s| s.to_owned());
        (drained, summary)
    };

    let available = ctx.config.context_window_tokens;
    let result = match ctx
        .strategy
        .compact(
            &drained,
            existing_summary.as_deref(),
            available,
            &ctx.client,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            ctx.store.lock().await.prepend_turns(drained);
            return Err(e.context("compaction failed; drained turns restored"));
        }
    };

    let mut guard = ctx.store.lock().await;
    guard.set_summary(result.summary);

    info!(
        strategy = ctx.strategy.name(),
        turns_compacted = result.turns_compacted,
        summary_tokens = result.summary_tokens,
        "compacted turns"
    );

    Ok(true)
}

fn format_deferred_notifications(notifications: &[DeferredNotification]) -> String {
    let mut parts = Vec::new();
    for n in notifications {
        match n {
            DeferredNotification::Approved { request_id, summary } => {
                parts.push(format!(
                    "Deferred action {request_id} (\"{summary}\") has been approved. \
                     Call approval_redeem with this request_id to execute."
                ));
            }
            DeferredNotification::Denied { request_id, summary, reason } => {
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
        Self { calls: BTreeMap::new() }
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
