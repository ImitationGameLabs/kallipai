//! kallip-run: post a prompt to a tagma agent and observe its run.
//!
//! Non-interactive CLI that targets the tagma's single root agent (or an
//! explicit agent via `--agent`), posts the prompt, streams the agent's
//! procedure (reasoning, tool calls, results) to stderr in `--verbose`, and
//! exits with a semantic exit code when the agent goes idle (calls `break`) or
//! hits a terminal error state. It is a runtime-telemetry observer: it does
//! **not** print the agent's message — a message is now a deliberate
//! `kallip lesche send` CLI call addressed to the user over the relay/chat
//! path, not a value on this stream. Use `--json` for a single machine-readable
//! object (`agentId`, `exit`).
//!
//! The target is the tagma-owned singleton root (or the `--agent` id); it
//! persists after the run. Per-run isolation is no longer provided — separate
//! runs share the root's context. For an isolated run, point `--agent` at a
//! dedicated subagent.

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use futures_util::{Stream, StreamExt};
use kallip_client::TagmaClient;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::SseEvent;

#[derive(Parser)]
#[command(
    name = "kallip-run",
    version,
    about = "Post a prompt to an agent and observe its run"
)]
struct Cli {
    /// The prompt to send to the agent.
    #[arg(long)]
    prompt: String,
    /// Target an explicit agent by id instead of the tagma's root agent.
    #[arg(long)]
    agent: Option<AgentId>,
    /// Emit a single JSON object on stdout: {agentId, exit}.
    /// Diagnostics still go to stderr.
    #[arg(long)]
    json: bool,
    /// Stream the agent's full procedure (reasoning, tool calls, results) to
    /// stderr. Off by default — the tagma persists execution history. With
    /// --json, the procedure streams to stderr; the JSON object is unchanged.
    #[arg(long)]
    verbose: bool,
}

/// Semantic exit codes for `kallip-run`.
///
/// Mapped to process exit codes via `#[repr(u8)]`:
/// 0 = success (agent went idle), 1 = error, 2 = max rounds exceeded,
/// 3 = cancelled, 4 = token budget exceeded, 5 = failover chain exhausted.
///
/// Serialized to the JSON `"exit"` field as a snake_case string
/// (`"success"`, `"max_rounds"`, …) via [`serde::Serialize`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
enum RunExit {
    Success = 0,
    Error = 1,
    MaxRounds = 2,
    Cancelled = 3,
    BudgetExceeded = 4,
    FailoverChainExhausted = 5,
}

impl From<RunExit> for ExitCode {
    fn from(code: RunExit) -> Self {
        ExitCode::from(code as u8)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            RunExit::Error.into()
        }
    }
}

async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let client = TagmaClient::from_env()?;

    // Output shape is driven by two explicit flags (no TTY auto-detection):
    // the default is minimal — a completion hint on stderr; `--verbose` streams
    // the agent's procedure to stderr; `--json` emits a single object on stdout.
    // No message is printed: the agent addresses the user via
    // `kallip lesche send`, not this stream.
    let (json, verbose) = (cli.json, cli.verbose);

    // Resolve the target agent: an explicit `--agent` id, or the tagma's
    // singleton root. Subscribe BEFORE posting: the tagma's SSE channel does
    // not replay past events to late subscribers, so a warm agent could emit
    // before we connect.
    let id = match &cli.agent {
        Some(existing) => existing.clone(),
        None => client.get_root_agent().await?.id,
    };
    let stream = client.event_stream(&id).await?;
    let resp = client.post_message(&id, &cli.prompt).await?;
    // Diagnostics belong on stderr regardless of --json (which only governs
    // stdout): a queue warning must still be visible.
    if let Some(warning) = resp.warning {
        eprintln!("warning: {warning}");
    }
    let exit = consume_stream(stream, verbose).await;

    if json {
        let obj = JsonObject {
            agent_id: &id,
            exit,
        };
        println!("{}", serde_json::to_string(&obj)?);
    } else {
        // Completion hint on stderr. A leading blank line separates it from the
        // streamed procedure in --verbose mode.
        let sep = if verbose { "\n" } else { "" };
        let status = match exit {
            RunExit::Success => "went idle",
            RunExit::MaxRounds => "hit max rounds",
            RunExit::Cancelled => "cancelled",
            RunExit::BudgetExceeded => "exceeded token budget",
            RunExit::FailoverChainExhausted => "failover chain exhausted",
            RunExit::Error => "errored",
        };
        eprintln!(
            "{sep}agent {id} {status}. Continue with: \
             kallip-run --agent {id} --prompt \"<prompt>\""
        );
    }

    Ok(exit.into())
}

/// End the current reasoning block, printing a trailing newline if one was
/// active on stderr.
fn end_reasoning(in_reasoning: &mut bool) {
    if *in_reasoning {
        eprintln!();
        *in_reasoning = false;
    }
}

/// JSON object emitted in `--json` mode. camelCase matches the project's JSON
/// conventions (see `SseEvent`'s `rename_all = "camelCase"`).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonObject<'a> {
    agent_id: &'a AgentId,
    exit: RunExit,
}

/// Consume the agent's SSE stream until a terminal event arrives.
///
/// This is a telemetry observer: it streams the agent's procedure (`[reasoning]`
/// / `[tool]` / …) to stderr when `verbose` is set, and returns the semantic
/// [`RunExit`] at the terminal event. It does **not** print any message — the
/// agent addresses the user via the `kallip lesche send` CLI over the
/// relay/chat path, not this stream. Diagnostics (warnings, errors) always go
/// to stderr.
///
/// Defaults to [`RunExit::Error`] if the stream closes without a terminal
/// event (tagma crash, network drop). Generic over the stream so [`run`] can
/// pass the already-open stream without naming the concrete `JsonEventStream`
/// type.
async fn consume_stream<S, E>(mut stream: S, verbose: bool) -> RunExit
where
    S: Stream<Item = Result<SseEvent, E>> + Unpin,
    E: std::fmt::Display,
{
    // Whether a `[reasoning] …` block is currently open on stderr.
    let mut in_reasoning = false;

    while let Some(result) = stream.next().await {
        let event = match result {
            Ok(event) => event,
            Err(e) => {
                end_reasoning(&mut in_reasoning);
                eprintln!("SSE error: {e}");
                return RunExit::Error;
            }
        };
        // Terminal events end this one-shot run. Classified by the wire-side
        // predicate (single source); the match below maps each terminal
        // variant to its RunExit and operator-facing summary. No silent `_`
        // catch-all for terminals: a future terminal variant without a
        // mapping fails loudly here instead of being suppressed.
        if event.is_terminal() {
            end_reasoning(&mut in_reasoning);
            return match event {
                SseEvent::Idle => {
                    // The agent yielded control (called `break`, or was force-idled).
                    // Terminal for this run: success.
                    RunExit::Success
                }
                SseEvent::Error { message } => {
                    eprintln!("{message}");
                    RunExit::Error
                }
                SseEvent::MaxRoundsExceeded => {
                    eprintln!("max rounds exceeded");
                    RunExit::MaxRounds
                }
                SseEvent::Cancelled => {
                    eprintln!("cancelled");
                    RunExit::Cancelled
                }
                // The round was interrupted; the tagma-side agent stays alive, but this
                // one-shot run is over. Treat like Cancelled.
                SseEvent::Interrupted => {
                    eprintln!("interrupted");
                    RunExit::Cancelled
                }
                SseEvent::TokenBudgetExceeded { consumed, budget } => {
                    eprintln!("token budget exceeded (consumed: {consumed}, budget: {budget})");
                    RunExit::BudgetExceeded
                }
                SseEvent::FailoverChainExhausted { reason, detail } => {
                    eprintln!("failover chain exhausted ({reason}): {detail}");
                    RunExit::FailoverChainExhausted
                }
                _ => unreachable!("terminal event {event:?} has no RunExit mapping"),
            };
        }
        match event {
            // Reasoning is streamed (verbose) but carries no terminal signal.
            SseEvent::ReasoningDelta { delta } => {
                if verbose {
                    if !in_reasoning {
                        eprint!("[reasoning] ");
                        in_reasoning = true;
                    }
                    eprint!("{delta}");
                }
            }
            // The agent's bare assistant text is telemetry here (verbose only) —
            // it is NOT a user message. A user message is a deliberate
            // `kallip lesche send` call.
            SseEvent::AssistantContentDelta { delta } => {
                if verbose {
                    if !in_reasoning {
                        eprint!("[assistant] ");
                    }
                    eprint!("{delta}");
                }
            }
            // Full (non-delta) events — defensive; the runtime emits deltas.
            SseEvent::Reasoning { content } => {
                if verbose {
                    end_reasoning(&mut in_reasoning);
                    eprintln!("[reasoning] {content}");
                }
            }
            SseEvent::AssistantContent { content } => {
                if verbose {
                    end_reasoning(&mut in_reasoning);
                    eprintln!("[assistant] {content}");
                }
            }
            SseEvent::ToolCall { name, .. } => {
                end_reasoning(&mut in_reasoning);
                if verbose {
                    eprintln!("[tool] {name}");
                }
            }
            SseEvent::ToolResult { result } => {
                end_reasoning(&mut in_reasoning);
                if verbose {
                    eprintln!("[tool-result] {result}");
                }
            }
            SseEvent::Retrying {
                attempt,
                max_attempts,
                error,
                delay_secs,
            } => {
                end_reasoning(&mut in_reasoning);
                if verbose {
                    eprintln!(
                        "[retry {attempt}/{max_attempts}] {error} (waiting {delay_secs:.1}s)"
                    );
                }
            }
            SseEvent::Failover { from, to, reason } => {
                end_reasoning(&mut in_reasoning);
                if verbose {
                    eprintln!("[failover] {from} → {to}: {reason}");
                }
            }
            SseEvent::StreamReset {
                error,
                attempt,
                max_attempts,
                delay_secs,
            } => {
                // The stream dropped mid-way and the runtime is retrying from scratch.
                // Nothing accumulated here to void (no reply buffer); just surface it.
                end_reasoning(&mut in_reasoning);
                if verbose {
                    eprintln!(
                        "[stream reset {attempt}/{max_attempts}] {error} (retrying in {delay_secs:.1}s)"
                    );
                }
            }
            // Suppress state-transition/informational events.
            SseEvent::Busy | SseEvent::Status { .. } | SseEvent::ApprovalUpdated { .. } => {}
            // Terminal variants already returned above; listed explicitly (not
            // folded into a `_`) so adding a terminal variant without touching
            // this match is a compile error, never a silent suppress.
            SseEvent::Idle
            | SseEvent::MaxRoundsExceeded
            | SseEvent::Error { .. }
            | SseEvent::Cancelled
            | SseEvent::Interrupted
            | SseEvent::TokenBudgetExceeded { .. }
            | SseEvent::FailoverChainExhausted { .. } => {
                unreachable!("terminal event {event:?} handled above")
            }
        }
    }

    // The stream ended without a terminal event (tagma crash, network drop).
    end_reasoning(&mut in_reasoning);
    eprintln!("stream ended without a terminal event");

    RunExit::Error
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use kallip_common::protocol::FailoverChainExhaustion;

    // Convenience alias: the stream items are `Result<SseEvent, E>` for any
    // `E: Display`; tests use `std::io::Error` as a stand-in error type.
    type Item = Result<SseEvent, std::io::Error>;

    #[tokio::test]
    async fn idle_marks_success() {
        // The agent yielded control (called `break`): terminal success. No reply
        // is captured — kallip-run is a telemetry observer.
        let events: Vec<Item> = vec![
            Ok(SseEvent::ReasoningDelta {
                delta: "thinking".into(),
            }),
            Ok(SseEvent::AssistantContentDelta {
                delta: "narration".into(),
            }),
            Ok(SseEvent::Idle),
        ];
        let exit = consume_stream(stream::iter(events), false).await;
        assert_eq!(exit, RunExit::Success);
    }

    #[tokio::test]
    async fn stream_reset_does_not_change_exit_before_terminal() {
        // A mid-stream drop is non-terminal telemetry now (no reply buffer to void);
        // the run continues until a terminal event.
        let events: Vec<Item> = vec![
            Ok(SseEvent::AssistantContentDelta {
                delta: "partial".into(),
            }),
            Ok(SseEvent::StreamReset {
                error: "connection reset".into(),
                attempt: 1,
                max_attempts: 2,
                delay_secs: 0.0,
            }),
            Ok(SseEvent::Idle),
        ];
        let exit = consume_stream(stream::iter(events), false).await;
        assert_eq!(exit, RunExit::Success);
    }

    #[tokio::test]
    async fn defaults_to_error_without_terminal_event() {
        let events: Vec<Item> = vec![Ok(SseEvent::ReasoningDelta { delta: "x".into() })];
        let exit = consume_stream(stream::iter(events), false).await;
        assert_eq!(exit, RunExit::Error);
    }

    #[tokio::test]
    async fn terminal_events_map_to_correct_exit() {
        let cases: [(SseEvent, RunExit); 6] = [
            (SseEvent::Idle, RunExit::Success),
            (
                SseEvent::Error {
                    message: String::new(),
                },
                RunExit::Error,
            ),
            (SseEvent::MaxRoundsExceeded, RunExit::MaxRounds),
            (SseEvent::Cancelled, RunExit::Cancelled),
            (
                SseEvent::TokenBudgetExceeded {
                    consumed: 1,
                    budget: 1,
                },
                RunExit::BudgetExceeded,
            ),
            (
                SseEvent::FailoverChainExhausted {
                    reason: FailoverChainExhaustion::NoFailoverConfigured,
                    detail: String::new(),
                },
                RunExit::FailoverChainExhausted,
            ),
        ];
        for (event, expected) in cases {
            let events: Vec<Item> = vec![Ok(event)];
            let exit = consume_stream(stream::iter(events), false).await;
            assert_eq!(exit, expected);
        }
    }
}
