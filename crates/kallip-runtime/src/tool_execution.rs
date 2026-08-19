//! Tool-call execution for the agent round loop.
//!
//! Executes the assistant's tool calls for one round: applies the per-tool timeout
//! exemption, runs the calls serially with the stop-on-first-failure discipline, gives
//! `break` its control-flow semantics, and synthesizes a result for every unanswered call
//! so the recorded turn stays protocol-valid. The round loop in `crate::runner` calls
//! [`execute_tool_calls`] and dispatches on [`ToolExecResult`].

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::acquisition::StreamConsumed;
use crate::agent_task::AgentContext;
use crate::event::AgentEvent;
use crate::policy::{ToolCallOutcome, error_result, skipped_tool_result, timed_out_tool_result};
use crate::runner::BreakUntil;
use crate::tools::DEFAULT_BREAK_TIMEOUT_SECS;
use just_llm_client::types::chat::{ChatMessage, ToolCallsMessage};

// ---------------------------------------------------------------------------
// Tool-call execution
// ---------------------------------------------------------------------------

/// Outcome of executing the assistant's tool calls.
pub(crate) enum ToolExecResult {
    /// The assembled turn messages (the assistant tool-call message + tool results).
    Messages(Vec<ChatMessage>),
    /// The agent called `break`. Carries the turn messages accumulated from the
    /// calls *before* `break` (the assistant tool-call message + any prior results)
    /// so the caller records them — `break` must not drop real work done earlier in
    /// the round — plus the parsed park target (`until`/`timeout_secs` args).
    /// `break`'s own result (and that of any call emitted after it, which the
    /// round loop never reaches) is synthesized by `synthesize_unanswered_results`
    /// so the recorded turn stays protocol-valid; the SSE ack still fires for UI.
    Break(Vec<ChatMessage>, BreakUntil),
    /// Cancelled mid-execution; partial results are dropped (mirrors the original early-return).
    Cancelled,
}

/// Tools that enforce their own bounded execution and are therefore exempt
/// from the outer `tool_timeout` wrapper: `bash_exec` bounds itself with its
/// internal per-call timeout (default 120s; the tool rejects requests over
/// 24h) precisely so a legitimately long call converts to a background task at
/// timeout instead of being killed here. The round cancel (below) still
/// applies, so a shutdown is never blocked. Note the approval_redeem route
/// is NOT exempt: the runner sees a redeemed call under the redeem tool's
/// name, and exempting it would unbound every redeemed tool.
const OWNS_TIMEOUT_TOOLS: &[&str] = &["bash_exec"];

/// Run one tool call, applying the outer timeout only when the tool does
/// not own its own bound (see [`OWNS_TIMEOUT_TOOLS`]).
pub(crate) async fn run_tool_bounded<F>(
    tool_name: &str,
    tool_timeout: Duration,
    fut: F,
) -> ToolCallOutcome
where
    F: Future<Output = ToolCallOutcome>,
{
    if OWNS_TIMEOUT_TOOLS.contains(&tool_name) {
        fut.await
    } else {
        match tokio::time::timeout(tool_timeout, fut).await {
            Ok(outcome) => outcome,
            Err(_) => {
                ToolCallOutcome::Failed(timed_out_tool_result(tool_name, tool_timeout.as_secs()))
            }
        }
    }
}

/// Execute the assistant's tool calls, emitting events and assembling the turn messages. On a
/// mid-call cancel returns `Cancelled` *before* the approval-state drain (mirrors the original),
/// dropping any partial results — the caller does not record the turn.
///
/// The assistant `ToolCalls` message clones `tool_calls` before the move-iterate loop consumes
/// them, so both the recorded assistant turn and the per-call dispatch see the full set.
pub(crate) async fn execute_tool_calls(
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

    // Stop on the first call that does not cleanly succeed. The agent composed
    // this round's calls without seeing intermediate results (a returned cwd,
    // exit code, etc.), so running later calls -- which may be destructive -- on
    // an unverified premise is unsafe. Once a call fails or is deferred pending
    // approval, the remaining calls are returned as synthetic skip errors; the
    // agent re-issues them after reviewing what happened.
    let mut skip: Option<(String, String)> = None;

    for call in consumed.tool_calls {
        // `break` is a control-flow primitive, not a normal tool: hoist its check
        // above the skip branch so it always terminates the round — even when an
        // earlier call armed `skip` (e.g. a deferred bash_exec). Calls issued
        // before `break` already ran and their results are in `turn_messages`;
        // calls issued after `break` never reach the loop body. `break` itself —
        // and any such trailing call — still needs a persisted tool result so the
        // recorded turn is protocol-valid (an assistant `tool_calls` message must
        // be followed by a tool result for every id). `synthesize_unanswered_results`
        // fills those in (break -> its real success ack; trailing calls -> a
        // not-executed error). The SSE ack below still fires for UI symmetry.
        if call.function.name == "break" {
            let until = parse_break_args(&call.function.arguments);
            tx.send(AgentEvent::ToolCall {
                name: "break".into(),
                args: call.function.arguments.clone(),
            })
            .await
            .ok();
            tx.send(AgentEvent::ToolResult(break_ack(until))).await.ok();
            synthesize_unanswered_results(&mut turn_messages, until);
            return ToolExecResult::Break(turn_messages, until);
        }

        let result = if let Some((prior_name, reason)) = &skip {
            // Earlier call did not cleanly succeed: do not execute this one.
            skipped_tool_result(&call.function.name, prior_name, reason)
        } else {
            tx.send(AgentEvent::ToolCall {
                name: call.function.name.clone(),
                args: call.function.arguments.clone(),
            })
            .await
            .ok();
            let outcome = {
                let tool_fut = run_tool_bounded(
                    &call.function.name,
                    tool_timeout,
                    ctx.executor
                        .execute(&call.function.name, &call.function.arguments),
                );
                tokio::select! {
                    result = tool_fut => result,
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

            // Record the result envelope; if it was not a clean success, arm
            // the skip flag so the rest of the round is skipped.
            match outcome {
                ToolCallOutcome::Success(s) => s,
                ToolCallOutcome::Failed(s) => {
                    skip = Some((call.function.name.clone(), "did not succeed".to_string()));
                    s
                }
                ToolCallOutcome::Deferred(s) => {
                    skip = Some((
                        call.function.name.clone(),
                        "is pending approval".to_string(),
                    ));
                    s
                }
            }
        };

        tx.send(AgentEvent::ToolResult(result.clone())).await.ok();
        turn_messages.push(ChatMessage::tool_result(result, call.id));
    }

    // Defensive: the loop above answers every call it iterates, so this is a
    // no-op on the Messages path. Kept as a safety net so no recorded turn can
    // ever carry an un-answered `tool_calls` id regardless of future changes.
    // (Defensive path: no `break` ran here, so the target is unreachable —
    // the defaults only exist to satisfy the signature.)
    synthesize_unanswered_results(
        &mut turn_messages,
        BreakUntil::Wait {
            timeout_secs: DEFAULT_BREAK_TIMEOUT_SECS,
        },
    );
    ToolExecResult::Messages(turn_messages)
}

/// Fill in a tool result for every `tool_calls` id in the round's assistant
/// message that has no matching `ToolResult` in the same turn.
///
/// The round's assistant tool-call message is `turn_messages[0]`; the remaining
/// messages are the tool results produced so far. `break` (and any call the model
/// emitted after `break`, which the round loop never reaches) would otherwise be
/// left without a result, producing a protocol-invalid turn: providers reject an
/// assistant `tool_calls` message not followed by a tool result for every id with
/// `400 insufficient tool messages following tool_calls message`.
///
/// Synthesis is honest about what happened: `break` parked (its real success ack),
/// and any other unanswered call never ran (a not-executed error). Idempotent — a
/// turn whose ids are all answered is untouched.
pub(crate) fn synthesize_unanswered_results(
    turn_messages: &mut Vec<ChatMessage>,
    until: BreakUntil,
) {
    // Snapshot the declared calls (id + tool name) off the assistant message
    // before the mutable borrow below.
    let Some(declared) = turn_messages
        .first()
        .and_then(|msg| msg.tool_calls())
        .map(|calls| {
            calls
                .iter()
                .map(|c| (c.id.clone(), c.function.name.clone()))
                .collect::<Vec<_>>()
        })
    else {
        return;
    };

    for (id, name) in declared {
        let answered = turn_messages
            .iter()
            .skip(1)
            .filter_map(|msg| msg.tool_call_id())
            .any(|answered_id| answered_id == id.as_str());
        if answered {
            continue;
        }
        let content = if name == "break" {
            break_ack(until)
        } else {
            error_result(
                &name,
                "not executed: a 'break' in this round terminated execution before this call ran"
                    .to_owned(),
            )
        };
        turn_messages.push(ChatMessage::tool_result(content, id));
    }
}

/// Parse the `break` tool's arguments into the park target. Lenient by design:
/// `break` is control flow, and refusing to break because of a malformed
/// argument would trap a finished agent in the round loop — the wrong failure
/// side. Anything unrecognized falls back to the defaults (`wait`,
/// `DEFAULT_BREAK_TIMEOUT_SECS`), so a typo'd `until` parks the agent with the
/// timer fuse instead of not parking at all.
fn parse_break_args(raw: &str) -> BreakUntil {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(raw) else {
        return BreakUntil::Wait {
            timeout_secs: DEFAULT_BREAK_TIMEOUT_SECS,
        };
    };
    let until = match args.get("until").and_then(|v| v.as_str()) {
        Some("idle") => BreakUntil::Idle,
        _ => BreakUntil::Wait {
            timeout_secs: DEFAULT_BREAK_TIMEOUT_SECS,
        },
    };
    match until {
        BreakUntil::Idle => BreakUntil::Idle,
        BreakUntil::Wait { .. } => {
            let timeout_secs = args
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .filter(|&t| t >= 1)
                .unwrap_or(DEFAULT_BREAK_TIMEOUT_SECS);
            BreakUntil::Wait { timeout_secs }
        }
    }
}

/// Normal success result for the `break` control-flow tool: the agent parked.
/// Echoes the effective park target (what the runtime will actually do, i.e.
/// defaults already applied) so the model sees the resolved semantics —
/// emitted as an SSE event for UI symmetry and persisted as the tool result
/// for the `break` call (via [`synthesize_unanswered_results`]) so the
/// recorded turn stays protocol-valid.
fn break_ack(until: BreakUntil) -> String {
    match until {
        BreakUntil::Idle => {
            r#"{"ok":true,"tool_name":"break","result":{"parked":true,"until":"idle"}}"#.to_owned()
        }
        BreakUntil::Wait { timeout_secs } => format!(
            r#"{{"ok":true,"tool_name":"break","result":{{"parked":true,"until":"wait","timeout_secs":{timeout_secs}}}}}"#
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `break` argument contract: defaults (`{}` → wait/600), explicit
    /// values pass through, and anything malformed or unrecognized falls back
    /// to the wait defaults — refusing to break would trap a finished agent.
    #[test]
    fn parse_break_args_contract() {
        let cases: &[(&str, BreakUntil)] = &[
            ("{}", BreakUntil::Wait { timeout_secs: 600 }),
            ("not json", BreakUntil::Wait { timeout_secs: 600 }),
            (r#"{"until":"idle"}"#, BreakUntil::Idle),
            (
                r#"{"until":"wait"}"#,
                BreakUntil::Wait { timeout_secs: 600 },
            ),
            (
                r#"{"until":"typo"}"#,
                BreakUntil::Wait { timeout_secs: 600 },
            ),
            (
                r#"{"until":"wait","timeout_secs":30}"#,
                BreakUntil::Wait { timeout_secs: 30 },
            ),
            (
                r#"{"until":"wait","timeout_secs":0}"#,
                BreakUntil::Wait { timeout_secs: 600 },
            ),
            (
                r#"{"until":"wait","timeout_secs":"soon"}"#,
                BreakUntil::Wait { timeout_secs: 600 },
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(parse_break_args(raw), *want, "args: {raw}");
        }
    }

    /// The ack echoes the *resolved* target so the model sees the effective
    /// semantics (defaults applied), on both the SSE event and the persisted
    /// tool result.
    #[test]
    fn break_ack_echoes_resolved_target() {
        let idle = break_ack(BreakUntil::Idle);
        assert!(idle.contains(r#""until":"idle""#), "{idle}");
        assert!(!idle.contains("timeout_secs"), "{idle}");

        let wait = break_ack(BreakUntil::Wait { timeout_secs: 30 });
        assert!(wait.contains(r#""until":"wait""#), "{wait}");
        assert!(wait.contains(r#""timeout_secs":30"#), "{wait}");
    }
}
