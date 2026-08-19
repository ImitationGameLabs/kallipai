//! Agent round execution loop.
//!
//! Owns the per-round orchestration: drain interjections, gate the context budget via
//! [`crate::budget_gate`], then acquire and consume the LLM stream via [`crate::acquisition`]
//! and execute tool calls via [`crate::tool_execution`]. The context-budget primitives
//! (estimation, warning injection, compaction) live in [`crate::context`]; stream acquisition
//! and failover driving live in [`crate::acquisition`]; the [`crate::failover::FailoverState`]
//! state machine stays pure in its own module.

use std::time::Duration;

use anyhow::{Result, bail};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::acquisition::{AcquireResult, acquire_stream};
use crate::agent_task::AgentContext;
use crate::approval::format_approval_notifications;
use crate::budget_gate::{BudgetAction, enforce_post_stream_budget, enforce_pre_call_budget};
use crate::context::{compose_context, estimate_context_tokens};
use crate::event::{AgentEvent, AgentOutcome};
use crate::tool_execution::{ToolExecResult, execute_tool_calls};
use just_llm_client::types::chat::ChatMessage;

// ---------------------------------------------------------------------------
// Round-loop control-flow signals
// ---------------------------------------------------------------------------

/// Where a `break` parks the agent (the `break` tool's `until` parameter).
/// `Wait` arms the wake timer (default); `Idle` finishes for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BreakUntil {
    Idle,
    Wait { timeout_secs: u64 },
}

/// Outcome of one invocation of the round loop. The outer state machine in
/// `agent_task::run_and_report` decides what to do next — this only reports what
/// happened in this round.
#[derive(Debug)]
pub(crate) enum RoundOutcome {
    /// The agent called `break`. The turn (including any tool calls preceding
    /// `break`) is already recorded inside the loop; the task parks as Idle
    /// (`until: "idle"`) or Waiting (armed wake timer, the default).
    Break(BreakUntil),
    /// A bare assistant response — content but no tool calls and no `break`. This
    /// no longer terminates the run: the outer loop injects a heartbeat prompt and
    /// re-loops, or force-idles via the no-progress guardrail. The assistant turn
    /// is **not** recorded yet; the outer loop records it (then the heartbeat) only
    /// when it decides to continue.
    BareAssistant { content: String },
    /// Park now and surface this outcome (chain exhausted / cancelled / budget / max rounds).
    /// Distinct from `Break`: these are non-deliberate park reasons, each surfacing its
    /// own event so the operator keeps the signal.
    Park(AgentOutcome),
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
) -> Result<RoundOutcome> {
    let tool_timeout = Duration::from_secs(ctx.config.tool_timeout_secs);

    for round in 0..ctx.config.max_tool_rounds {
        drain_interjections(ctx, prompt_rx).await;

        // -- Pre-call token budget check (shared tree-wide counter) --
        let snap = ctx.token_budget.snapshot();
        if snap.is_exceeded() {
            return Ok(RoundOutcome::Park(AgentOutcome::TokenBudgetExceeded {
                consumed: snap.consumed,
                budget: snap.budget,
            }));
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
                _ = round_cancel.cancelled() => return Ok(RoundOutcome::Park(AgentOutcome::Cancelled)),
            }
        };

        match enforce_pre_call_budget(ctx, prompt_tokens, round_cancel).await {
            BudgetAction::Recompose => continue,
            BudgetAction::Return(outcome) => return Ok(RoundOutcome::Park(outcome)),
            BudgetAction::Proceed => {}
        }

        // -- Within-tier failover acquisition (also consumes the stream, retrying mid-stream
        // transport drops in-place) --
        let consumed = match acquire_stream(ctx, messages, tools, tx, round_cancel, round).await {
            AcquireResult::Consumed(c) => c,
            AcquireResult::Outcome(outcome) => return Ok(RoundOutcome::Park(outcome)),
            AcquireResult::Error(e) => return Err(e),
        };

        match enforce_post_stream_budget(ctx, consumed.usage.as_ref()).await {
            BudgetAction::Recompose => continue,
            BudgetAction::Return(outcome) => return Ok(RoundOutcome::Park(outcome)),
            BudgetAction::Proceed => {}
        }

        // -- Bare assistant response (no tool calls): the agent did not call `break`,
        //    so this does NOT terminate the run. Surface the content to the outer
        //    loop, which injects a heartbeat prompt and re-loops (or force-idles via
        //    the no-progress guardrail). An empty response is malformed and errors. --
        if consumed.tool_calls.is_empty() {
            if consumed.content.is_empty() {
                bail!("assistant returned neither tool calls nor final content");
            }
            return Ok(RoundOutcome::BareAssistant {
                content: consumed.content,
            });
        }

        // -- Tool execution --
        let turn_messages = match execute_tool_calls(ctx, tx, consumed, tool_timeout, round_cancel)
            .await
        {
            ToolExecResult::Cancelled => return Ok(RoundOutcome::Park(AgentOutcome::Cancelled)),
            ToolExecResult::Break(msgs, until) => {
                // `break` parks as Idle, but the calls preceding it in this round
                // still produced real work — record it before yielding.
                ctx.record_turn(msgs).await;
                ctx.persist().await;
                return Ok(RoundOutcome::Break(until));
            }
            ToolExecResult::Messages(msgs) => msgs,
        };

        ctx.record_turn(turn_messages).await;
        ctx.persist().await;
    }

    Ok(RoundOutcome::Park(AgentOutcome::MaxRoundsExceeded))
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
#[cfg(test)]
mod tests;
