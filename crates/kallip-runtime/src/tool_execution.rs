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
use crate::config::{DEFAULT_TOOL_RESULT_FULL_TOKENS, DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS};
use crate::context::estimate_text;
use crate::event::AgentEvent;
use crate::policy::{ToolCallOutcome, error_result, skipped_tool_result, timed_out_tool_result};
use crate::runner::BreakUntil;
use crate::text_slice::converge_under_cap;
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

// ---------------------------------------------------------------------------
// Tool-result token cap
// ---------------------------------------------------------------------------

/// Cap a tool result's token size before it enters the recorded context.
///
/// No result gets special treatment — not even envelope-shaped ones. An
/// envelope over the cap means its own paging discipline failed, and cutting
/// it makes that loud instead of silently feeding the bloat into context.
/// Plain results at or under [`DEFAULT_TOOL_RESULT_FULL_TOKENS`] estimated
/// tokens pass through byte-for-byte. Anything larger is cut to a head+tail
/// slice of roughly [`DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS`] tokens plus a
/// banner stating what was kept and how to get at the rest — the feed side of
/// the 2026-09-06 compaction stall, where one clipped command result entered
/// context as a ~503K-token turn and wedged the compressor behind it.
fn cap_tool_result(result: String) -> String {
    let full_est = estimate_text(&result);
    if full_est <= DEFAULT_TOOL_RESULT_FULL_TOKENS {
        return result;
    }
    let total = result.chars().count();
    // Density-derived character cap, then re-derived from each cut's measured
    // density: the tokenx scan is linear, but measuring the actual cut is
    // cheaper than proving it so.
    let chars_cap = DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS * total / full_est;
    let parts = converge_under_cap(&result, chars_cap, DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS);
    with_banner(total, full_est, &parts)
}

/// Wrap the capped body with the truncation banner: what was kept, how big
/// the whole output was, and how to get at the rest. The gap itself is
/// declared by the marker inside the body — the banner covers the totals.
fn with_banner(total: usize, full_est: usize, parts: &(usize, usize, String)) -> String {
    let banner = format!(
        "{}\n[... truncated: kept first {} and last {} of {total} chars (~{} of ~{full_est} estimated tokens). Narrow the command's output and re-run to see other parts ...]",
        parts.2,
        parts.0,
        parts.1,
        estimate_text(&parts.2)
    );
    banner
}

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

        let result = cap_tool_result(result);
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

/// Tool calls declared in these messages that no tool result in the same
/// slice answers — `(id, tool name)` pairs, in declaration order.
///
/// This is the pairing invariant every recorded turn must satisfy: each id
/// an assistant message declares via `tool_calls` has a `ToolResult` message
/// with that `tool_call_id` somewhere in the slice. Scans all messages
/// rather than trusting the assistant-first round shape, so it doubles as a
/// damage probe over persisted turns (restore-time pairing validation).
pub(crate) fn unanswered_call_ids(messages: &[ChatMessage]) -> Vec<(String, String)> {
    let declared: Vec<(String, String)> = messages
        .iter()
        .filter_map(|msg| msg.tool_calls())
        .flatten()
        .map(|c| (c.id.clone(), c.function.name.clone()))
        .collect();
    let answered: Vec<&str> = messages
        .iter()
        .filter_map(|msg| msg.tool_call_id())
        .collect();
    declared
        .into_iter()
        .filter(|(id, _)| !answered.contains(&id.as_str()))
        .collect()
}

/// Tool results in these messages whose `tool_call_id` no declared call in
/// the same slice matches — the mirror damage of [`unanswered_call_ids`],
/// in message order.
pub(crate) fn orphan_result_ids(messages: &[ChatMessage]) -> Vec<String> {
    let declared: Vec<&str> = messages
        .iter()
        .filter_map(|msg| msg.tool_calls())
        .flatten()
        .map(|c| c.id.as_str())
        .collect();
    messages
        .iter()
        .filter_map(|msg| msg.tool_call_id())
        .filter(|rid| !declared.contains(rid))
        .map(str::to_owned)
        .collect()
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
    // Snapshot the declared-but-unanswered calls before the mutable push
    // below.
    let unanswered = unanswered_call_ids(turn_messages);

    for (id, name) in unanswered {
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
    use crate::test_support::{assistant_msg, tool_calls_msg, tool_result_msg, user_msg};

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

    /// The pairing probes agree on a well-formed round (all calls answered,
    /// no orphans) and each flags its own damage class: a declared call with
    /// no result, and a result answering nothing in the slice.
    #[test]
    fn pairing_probes_flag_both_damage_directions() {
        let clean = vec![
            tool_calls_msg(&[("c1", "read"), ("c2", "edit")]),
            tool_result_msg("ok", "c1"),
            tool_result_msg("ok", "c2"),
        ];
        assert!(unanswered_call_ids(&clean).is_empty());
        assert!(orphan_result_ids(&clean).is_empty());

        let damaged = vec![
            tool_calls_msg(&[("c1", "read"), ("c2", "edit")]),
            tool_result_msg("ok", "c1"),
            tool_result_msg("ghost", "c9"),
        ];
        assert_eq!(
            unanswered_call_ids(&damaged),
            vec![("c2".into(), "edit".into())]
        );
        assert_eq!(orphan_result_ids(&damaged), vec!["c9".to_owned()]);

        // No tool traffic at all: both probes quiet.
        let plain = vec![user_msg("hi"), assistant_msg("hello")];
        assert!(unanswered_call_ids(&plain).is_empty());
        assert!(orphan_result_ids(&plain).is_empty());
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;
    use kallip_common::toolresult::ToolResultEnvelope;

    #[test]
    fn line_structured_output_is_cut_on_line_boundaries() {
        // ~80 chars/line of prose: every line is far under the whole-line
        // threshold, so both cut sides must land on line boundaries.
        let mut text = String::new();
        for i in 0..3_000 {
            // Prose-plus-numbers filler: tokenx compresses runs of repeated
            // chars, so the gate fixture needs varied text to stay over the
            // full-result line.
            text.push_str(&format!(
                "L{i:04} line {i} value {} carries state and diagnostics for slice {}\n",
                i * 37,
                i * 91
            ));
        }
        assert!(estimate_text(&text) > DEFAULT_TOOL_RESULT_FULL_TOKENS);

        let capped = cap_tool_result(text.clone());
        assert!(
            capped.contains("chars omitted"),
            "mid-seam gap must be declared"
        );
        assert!(capped.starts_with("L0000 "));
        // The banner's numbers must describe the real keep: a refactor that
        // swapped in the raw 60/40 targets would still pass the landing check.
        let banner = &capped[capped.find("truncated: kept first").unwrap()..];
        let nums: Vec<usize> = banner
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        assert_eq!(
            nums[2],
            text.chars().count(),
            "banner total must be the output size"
        );
        assert!(nums[0] > nums[1], "head must keep more than the tail");
        assert!(
            nums[0] + nums[1] < nums[2],
            "kept plus omitted must not exceed the total"
        );
        let m = capped.find("\n[... ").unwrap();
        assert_eq!(
            capped.as_bytes()[m - 1],
            b'\n',
            "head must end at a line end"
        );
        let after = capped.find("chars omitted ...]\n").unwrap() + "chars omitted ...]\n".len();
        let tail = &capped[after..];
        assert!(tail.starts_with('L'), "tail must start at a line start");
        let line_end = tail.find('\n').unwrap() + 1;
        assert!(
            text.contains(&tail[..line_end]),
            "tail must begin with a complete source line"
        );
        assert!(
            estimate_text(&capped) <= DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS + 200,
            "kept estimate {} exceeded the cap",
            estimate_text(&capped)
        );
    }

    #[test]
    fn small_tool_result_passes_through_untouched() {
        let r = "plain small output".to_string();
        assert_eq!(cap_tool_result(r.clone()), r);
    }

    #[test]
    fn oversized_tool_result_is_cut_with_banner() {
        // CJK at tokenx's ~1 token/char keeps the premise deterministic.
        // Unique sentinels at both ends: with a uniformly repeated fixture the
        // head and tail assertions below would pass even if the cut kept only
        // one side of the output.
        let big = format!("HEAD{}TAIL", "错".repeat(50_000));
        assert!(estimate_text(&big) > DEFAULT_TOOL_RESULT_FULL_TOKENS);

        let capped = cap_tool_result(big.clone());
        assert!(capped.contains("truncated"), "banner must be present");
        assert!(
            capped.contains("chars omitted"),
            "mid-seam gap must be declared"
        );
        assert!(capped.starts_with("HEAD"), "head sentinel must be kept");
        assert!(capped.contains("TAIL"), "tail sentinel must be kept");
        // The cap covers the banner too: everything over it is the marker text.
        assert!(
            estimate_text(&capped) <= DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS + 200,
            "kept estimate {} exceeded the cap",
            estimate_text(&capped)
        );
    }

    #[test]
    fn oversized_envelope_is_cut_like_any_other() {
        // An envelope over the cap means its own paging discipline failed;
        // the cut makes that loud instead of silently feeding the bloat.
        let env = ToolResultEnvelope {
            ok: true,
            tool_name: "bash_exec".to_string(),
            result: Some(serde_json::json!({ "out": "错".repeat(50_000) })),
            error: None,
            pending_approval: None,
            rest: Default::default(),
        };
        let s = serde_json::to_string(&env).unwrap();
        let capped = cap_tool_result(s);
        assert!(
            capped.contains("truncated"),
            "oversized envelope must be cut"
        );
        assert!(
            estimate_text(&capped) <= DEFAULT_TOOL_RESULT_TRUNCATED_TOKENS + 200,
            "cut envelope must land at the cap"
        );
    }
}
