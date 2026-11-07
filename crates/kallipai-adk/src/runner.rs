//! Agent round execution loop.
//!
//! Owns the per-round orchestration: drain interjections, gate the context budget via
//! [`crate::budget_gate`], then acquire and consume the LLM stream via [`crate::acquisition`]
//! and execute tool calls via [`crate::tool_execution`]. The context-budget primitives
//! (estimation, warning injection, compaction) live in [`crate::context`]; stream acquisition
//! and failover driving live in [`crate::acquisition`]; the [`crate::failover::FailoverState`]
//! state machine stays pure in its own module.

use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info, warn};

use crate::acquisition::{
    AcquireResult, acquire_stream, degrade_on_image_rejection, release_unspent_window,
};
use crate::agent_task::{AgentContext, cap_external_message};
use crate::approval::format_approval_notifications;
use crate::budget_gate::{BudgetAction, enforce_post_stream_budget, enforce_pre_call_budget};
use crate::context::{compose_context, estimate_context_tokens};
use crate::event::{AgentEvent, AgentOutcome};
use crate::history::TurnAttempt;
use crate::tool_execution::{ToolExecResult, execute_tool_calls};
use just_llm_client::types::generation::Message;
use kallipai_common::telemetry::{Settle, record_attribute};

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
    ///
    /// `reasoning` is the model's own thinking for this turn: the outer loop
    /// records it with the message so a later request can pass it back.
    /// `attempt` is what served the round — the profile in flight, the usage the
    /// backend reported and the round clock — recorded on the same turn so the
    /// log carries the model face, not just its words.
    BareAssistant {
        content: String,
        reasoning: String,
        attempt: Box<TurnAttempt>,
    },
    /// Park now and surface this outcome (chain exhausted / cancelled / budget / max rounds).
    /// Distinct from `Break`: these are non-deliberate park reasons, each surfacing its
    /// own event so the operator keeps the signal.
    Park(AgentOutcome),
}

// ---------------------------------------------------------------------------
// Agent round loop
// ---------------------------------------------------------------------------

/// Run the agent round loop until completion or max rounds.
///
/// Every exit gives an unspent window back to the table: a round can take a
/// window at a gate and then stop before the send that would spend it, and the
/// record it left must not pace the round after it.
pub(crate) async fn run_agent_rounds(
    ctx: &mut AgentContext,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
    round_cancel: &CancellationToken,
) -> Result<RoundOutcome> {
    let outcome = run_rounds(ctx, tx, prompt_rx, round_cancel).await;
    release_unspent_window(ctx);
    outcome
}

/// Which way one round sent the loop.
enum TurnStep {
    /// The round gave its window back; the loop goes on with the next round.
    Recompose,
    /// The round ran to its end; the loop starts the next one.
    Next,
    /// The round settled the run.
    Done(Result<RoundOutcome>),
}

/// The one word a round's span carries when the round failed.
const ROUND_FAILED: &str = "round_failed";

/// The one word a round's span carries for how the round ended. A round the
/// lifecycle cancelled does not come through here: it is settled as
/// `cancelled` where the round's span is closed.
fn turn_outcome_label(outcome: &RoundOutcome) -> &'static str {
    match outcome {
        RoundOutcome::Break(_) => "break",
        RoundOutcome::BareAssistant { .. } => "bare",
        RoundOutcome::Park(_) => "park",
    }
}

/// The round loop itself; [`run_agent_rounds`] wraps it so that every exit,
/// including an error, ends with the window accounted for.
///
/// Every round runs inside its own span. The span is entered here, around the
/// whole round body, because the body awaits gates that open spans of their
/// own: a span entered from inside the body would be lost across those awaits,
/// and the compaction call would hang off the trace root instead.
async fn run_rounds(
    ctx: &mut AgentContext,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
    round_cancel: &CancellationToken,
) -> Result<RoundOutcome> {
    let tool_timeout = Duration::from_secs(ctx.config.tool_timeout_secs);
    let role = ctx.config.role.clone();
    let agent = crate::spans::agent_label(ctx.config.agent_id.as_ref());

    for round in 0..ctx.config.max_tool_rounds {
        let turn = crate::spans::turn_span(&role, &agent, round);
        let settle = Settle::new(&turn);
        let step = run_round_body(ctx, tx, prompt_rx, round_cancel, round, tool_timeout, &turn)
            .instrument(turn.clone())
            .await;
        match step {
            TurnStep::Recompose => {
                record_attribute(&turn, "kallipai.turn.outcome", "recompose");
                settle.done();
            }
            TurnStep::Next => {
                record_attribute(&turn, "kallipai.turn.outcome", "continue");
                settle.done();
            }
            TurnStep::Done(Ok(outcome)) => {
                // A round the lifecycle cancelled is neither a failure nor a
                // round that finished: the word says which, so the trace can
                // tell a restart from work that went wrong.
                if matches!(outcome, RoundOutcome::Park(AgentOutcome::Cancelled)) {
                    record_attribute(&turn, "kallipai.turn.outcome", "cancelled");
                    settle.cancelled();
                } else {
                    record_attribute(&turn, "kallipai.turn.outcome", turn_outcome_label(&outcome));
                    settle.done();
                }
                return Ok(outcome);
            }
            TurnStep::Done(Err(error)) => {
                record_attribute(&turn, "kallipai.turn.outcome", "error");
                settle.failed(ROUND_FAILED);
                return Err(error);
            }
        }
    }

    Ok(RoundOutcome::Park(AgentOutcome::MaxRoundsExceeded))
}

/// One round: from the fresh episode to the record it leaves behind.
///
/// It reports how the loop goes on rather than `continue`-ing, so the caller
/// owns the loop and the round's span covers this whole future. `turn` is the
/// span, handed over to record what only the body knows (how many messages the
/// round sent, what the budget gate estimated): the parenting comes from the
/// caller's `instrument`, not from this handle.
#[allow(clippy::too_many_arguments)]
async fn run_round_body(
    ctx: &mut AgentContext,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    prompt_rx: &mut tokio::sync::mpsc::Receiver<String>,
    round_cancel: &CancellationToken,
    round: usize,
    tool_timeout: Duration,
    turn: &tracing::Span,
) -> TurnStep {
    // The round clock the history record carries: from here to the response
    // being consumed, tool execution excluded.
    let round_start = Instant::now();
    // A tool round is a new execution: its sends get a fresh budget and deadline.
    // This is the only per-round reset of the ledger; the prompt entry does not
    // seed one, so a round never inherits the previous round's sends.
    ctx.start_episode();
    drain_interjections(ctx, prompt_rx).await;

    // -- Pre-call token budget check (shared tree-wide counter) --
    let snap = ctx.token_budget.snapshot();
    if snap.is_exceeded() {
        return TurnStep::Done(Ok(RoundOutcome::Park(AgentOutcome::TokenBudgetExceeded {
            consumed: snap.consumed,
            budget: snap.budget,
        })));
    }

    // -- Approval notification injection --
    let notifications = ctx.approvals.lock().await.drain_notifications();
    if !notifications.is_empty() {
        let msg = format_approval_notifications(&notifications);
        ctx.record_turn(vec![Message::user(&msg)]).await;
    }

    // -- Context composition and token estimation --
    let messages = compose_context(ctx.store.clone()).await;
    let tools = ctx.store.lock().await.tool_definitions().to_vec();
    record_attribute(
        turn,
        "kallipai.turn.messages",
        (messages.len() as u64).to_string(),
    );
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
            _ = round_cancel.cancelled() => {
                return TurnStep::Done(Ok(RoundOutcome::Park(AgentOutcome::Cancelled)));
            }
        }
    };
    record_attribute(
        turn,
        "kallipai.turn.prompt_tokens",
        prompt_tokens.to_string(),
    );

    match enforce_pre_call_budget(ctx, prompt_tokens, round, round_cancel).await {
        BudgetAction::Recompose => return TurnStep::Recompose,
        BudgetAction::Return(outcome) => {
            return TurnStep::Done(Ok(RoundOutcome::Park(outcome)));
        }
        BudgetAction::Proceed => {}
        // A compaction failure no send can help with ends the round: the error
        // carries the verdict's message and parks the agent (see the caller).
        BudgetAction::Fail(error) => return TurnStep::Done(Err(error)),
    }

    // -- Within-set failover acquisition: consume the stream with mid-stream transport
    // retry in-place and failover on endpoint-level failures. A 4xx while the request
    // carried images triggers the runtime degrade (strip, retry once, invalidate on
    // success — see `degrade_on_image_rejection`). --
    let consumed = match acquire_stream(ctx, messages, tools.clone(), tx, round_cancel, round).await
    {
        AcquireResult::Consumed(c) => c,
        // A stop carries the whole verdict in its message: the round errors with
        // it directly. Degrading images and resending would re-send a request the
        // chain already ruled out (a quota rejection reads as a provider
        // rejection), so the stop must not go through that path.
        AcquireResult::Stop { message } => {
            return TurnStep::Done(Err(anyhow::Error::msg(message)));
        }
        AcquireResult::Outcome(outcome) => {
            return TurnStep::Done(Ok(RoundOutcome::Park(outcome)));
        }
        AcquireResult::Error(e) => {
            match degrade_on_image_rejection(ctx, &e, tools, tx, round_cancel, round).await {
                Some(AcquireResult::Consumed(c)) => c,
                Some(AcquireResult::Outcome(outcome)) => {
                    return TurnStep::Done(Ok(RoundOutcome::Park(outcome)));
                }
                Some(AcquireResult::Error(_)) | None => return TurnStep::Done(Err(e)),
                Some(AcquireResult::Stop { message }) => {
                    return TurnStep::Done(Err(anyhow::Error::msg(message)));
                }
            }
        }
    };

    match enforce_post_stream_budget(ctx, consumed.usage.as_ref()).await {
        BudgetAction::Recompose => return TurnStep::Recompose,
        BudgetAction::Return(outcome) => {
            return TurnStep::Done(Ok(RoundOutcome::Park(outcome)));
        }
        BudgetAction::Proceed => {}
        // The post-stream gate does not compact, so no failure verdict can
        // reach it; the arm keeps the shared verdict type total.
        BudgetAction::Fail(error) => return TurnStep::Done(Err(error)),
    }

    // Snapshot what served this round before the response moves into tool
    // execution: the profile in flight and the usage are gone after that
    // move; the round clock is already running.
    let attempt = ctx.turn_attempt(
        round,
        consumed.usage.as_ref(),
        round_start.elapsed().as_millis() as u64,
    );

    // -- Bare assistant response (no tool calls): the agent did not call `break`,
    //    so this does NOT terminate the run. Surface the content to the outer
    //    loop, which injects a heartbeat prompt and re-loops (or force-idles via
    //    the no-progress guardrail). An empty response is malformed and errors. --
    if consumed.tool_calls.is_empty() {
        if consumed.content.is_empty() {
            return TurnStep::Done(Err(anyhow::anyhow!(
                "assistant returned neither tool calls nor final content"
            )));
        }
        return TurnStep::Done(Ok(RoundOutcome::BareAssistant {
            content: consumed.content,
            reasoning: consumed.reasoning,
            attempt: Box::new(attempt),
        }));
    }

    // -- Tool execution --
    let turn_messages =
        match execute_tool_calls(ctx, tx, consumed, tool_timeout, round_cancel).await {
            ToolExecResult::Cancelled => {
                return TurnStep::Done(Ok(RoundOutcome::Park(AgentOutcome::Cancelled)));
            }
            ToolExecResult::Break(msgs, until) => {
                // `break` parks as Idle, but the calls preceding it in this round
                // still produced real work — record it before yielding.
                ctx.record_turn_with_attempt(msgs, attempt).await;
                ctx.persist().await;
                return TurnStep::Done(Ok(RoundOutcome::Break(until)));
            }
            ToolExecResult::Messages(msgs) => msgs,
        };

    ctx.record_turn_with_attempt(turn_messages, attempt).await;
    ctx.persist().await;
    TurnStep::Next
}

/// Consume queued interjections (prompts/commands) and record them as
/// a single turn. Each interjection individually passes the entry cap
/// (spill + cut with banner when oversized) before being wrapped.
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
            .map(|t| {
                format!(
                    "[Interjected message]\n{}\n[/Interjected message]",
                    cap_external_message(t, false)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        ctx.record_turn(vec![Message::user(&msg)]).await;
        info!(count = interjected.len(), "injected interjected messages");
    }
}
#[cfg(test)]
mod tests;
