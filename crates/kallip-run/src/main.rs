//! kallip-run: agent runner for scripting and benchmarking.
//!
//! Non-interactive CLI that creates an agent via the daemon (or resumes an
//! existing one with `--agent`), prints the final assistant reply to stdout,
//! and exits with a semantic exit code. Pass `--verbose` to stream the
//! agent's procedure (reasoning, tool calls) to stderr, or `--json` for a
//! single machine-readable object (`agentId`, `assistant`, `exit`,
//! `removed`). Designed for scripted and automated workflows.
//!
//! By default the agent is **preserved** after completion so that logs,
//! history, and token usage remain available for auditing, and so it can be
//! resumed with `--agent`. Pass `--remove` to archive the agent (history and
//! usage preserved) after the run finishes.

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use futures_util::{Stream, StreamExt};
use kallip_client::DaemonClient;
use kallip_common::agentid::AgentId;
use kallip_common::protocol::{CreateAgentRequest, MaxToolRounds, SseEvent};

#[derive(Parser)]
#[command(
    name = "kallip-run",
    version,
    about = "Create an agent, run it to completion, and print the result"
)]
struct Cli {
    /// The prompt to send to the agent (new run), or the follow-up message
    /// when resuming with `--agent`.
    #[arg(long)]
    prompt: String,
    /// Working directory for the agent (spawn only; ignored with `--agent`).
    #[arg(long)]
    workspace_root: Option<String>,
    /// Maximum tool-call rounds for this run (spawn only; ignored with `--agent`).
    /// Overrides the daemon default (unlimited unless KALLIP_MAX_TOOL_ROUNDS is set).
    #[arg(long)]
    max_rounds: Option<usize>,
    /// Resume an existing agent by id instead of spawning a new one.
    #[arg(long)]
    agent: Option<AgentId>,
    /// Emit a single JSON object on stdout: {agentId, assistant, exit, removed}.
    /// Diagnostics still go to stderr.
    #[arg(long)]
    json: bool,
    /// Stream the agent's full procedure (reasoning, tool calls, results) to
    /// stderr. Off by default — the daemon persists execution history. With
    /// --json, the procedure streams to stderr; the JSON object is unchanged.
    #[arg(long)]
    verbose: bool,
    /// Archive the agent (history and usage preserved) after completion.
    /// By default the agent is preserved and can be resumed with `--agent`.
    #[arg(long)]
    remove: bool,
}

/// Semantic exit codes for `kallip-run`.
///
/// Mapped to process exit codes via `#[repr(u8)]`:
/// 0 = success, 1 = error, 2 = max rounds exceeded,
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
    let client = DaemonClient::from_env()?;

    // Output shape is driven by two explicit flags (no TTY auto-detection):
    // the default is minimal — the final assistant reply on stdout plus a
    // completion hint on stderr.
    let (json, verbose) = (cli.json, cli.verbose);

    // Open the event stream and resolve the agent id. For a resume we subscribe
    // BEFORE posting the follow-up message: the daemon's SSE channel does not
    // replay past events to late subscribers, so a warm resumed agent could
    // otherwise emit before we connect. (Spawn embeds the initial prompt in the
    // create request, so its subscribe-after-create window is pre-existing and
    // mitigated by LLM latency.)
    let (id, outcome) = if let Some(existing) = &cli.agent {
        let id = existing.clone();
        let stream = client.event_stream(&id).await?;
        let resp = client.post_message(&id, &cli.prompt).await?;
        // Diagnostics belong on stderr regardless of --json (which only governs
        // stdout): a queue warning must still be visible.
        if let Some(warning) = resp.warning {
            eprintln!("warning: {warning}");
        }
        (id, consume_stream(stream, json, verbose).await)
    } else {
        let id = client
            .spawn(CreateAgentRequest {
                workspace_root: cli.workspace_root,
                skills: vec![],
                prompt: Some(cli.prompt),
                created_by: std::env::var("KALLIP_ID").ok().map(AgentId::from),
                role: String::new(),
                description: String::new(),
                max_tool_rounds: cli.max_rounds.map(MaxToolRounds::Limited),
            })
            .await?;
        let stream = client.event_stream(&id).await?;
        (id, consume_stream(stream, json, verbose).await)
    };

    let removed = if cli.remove {
        match client.remove_agent(&id).await {
            Ok(()) => true,
            Err(e) => {
                // Diagnostics belong on stderr regardless of --json: a failed
                // remove must not be silent (the JSON `removed` field is the
                // machine-readable signal, this is the human one).
                eprintln!("warning: failed to remove agent {id}: {e}");
                false
            }
        }
    } else {
        false
    };

    if json {
        let obj = JsonObject {
            agent_id: &id,
            assistant: &outcome.assistant,
            exit: outcome.exit,
            removed,
        };
        println!("{}", serde_json::to_string(&obj)?);
    } else {
        // Completion hint on stderr (text modes only). A leading blank line
        // separates it from the streamed procedure in --verbose mode; in the
        // minimal default the reply is on stdout, so no separator is needed.
        let sep = if verbose { "\n" } else { "" };
        if cli.remove {
            let msg = if removed {
                "archived"
            } else {
                "remove failed (see warning above)"
            };
            eprintln!("{sep}agent {id} {msg}.");
        } else {
            let status = match outcome.exit {
                RunExit::Success => "finished (kept)",
                RunExit::MaxRounds => "hit max rounds (kept)",
                RunExit::Cancelled => "cancelled (kept)",
                RunExit::BudgetExceeded => "exceeded token budget (kept)",
                RunExit::FailoverChainExhausted => "failover chain exhausted (kept)",
                RunExit::Error => "errored (kept)",
            };
            eprintln!(
                "{sep}agent {id} {status}. Continue with: \
                 kallip-run --agent {id} --prompt \"<prompt>\""
            );
        }
    }

    Ok(outcome.exit.into())
}

/// End the current reasoning block, printing a trailing newline if one was
/// active on stderr.
fn end_reasoning(in_reasoning: &mut bool) {
    if *in_reasoning {
        eprintln!();
        *in_reasoning = false;
    }
}

/// Result of consuming an event stream until a terminal event.
struct Outcome {
    exit: RunExit,
    assistant: String,
}

/// JSON object emitted in `--json` mode. camelCase matches the project's JSON
/// conventions (see `SseEvent`'s `rename_all = "camelCase"`).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonObject<'a> {
    agent_id: &'a AgentId,
    assistant: &'a str,
    exit: RunExit,
    removed: bool,
}

/// Consume the agent's SSE stream until a terminal event arrives.
///
/// Output is driven by `json` and `verbose`:
/// - The final assistant reply always goes to stdout (printed at `Finished`)
///   unless `json` is set, in which case it is carried in the JSON object
///   emitted by [`run`].
/// - `verbose` streams the procedure (`[reasoning]` / `[tool]` / …) to stderr
///   in any mode; without it the procedure is suppressed (the daemon persists
///   execution history).
/// - Diagnostics (warnings, errors) always go to stderr.
///
/// Defaults to [`RunExit::Error`] if the stream closes without a terminal
/// event (daemon crash, network drop). Generic over the stream so [`run`] can
/// pass the already-open stream without naming the concrete `JsonEventStream`
/// type.
async fn consume_stream<S, E>(mut stream: S, json: bool, verbose: bool) -> Outcome
where
    S: Stream<Item = Result<SseEvent, E>> + Unpin,
    E: std::fmt::Display,
{
    // Whether a `[reasoning] …` block is currently open on stderr.
    let mut in_reasoning = false;
    let mut assistant = String::new();
    let exit = RunExit::Error;

    while let Some(result) = stream.next().await {
        let event = match result {
            Ok(event) => event,
            Err(e) => {
                end_reasoning(&mut in_reasoning);
                eprintln!("SSE error: {e}");
                return Outcome {
                    exit: RunExit::Error,
                    assistant,
                };
            }
        };
        match event {
            // Reasoning is streamed (verbose) but never accumulated — it is not
            // part of the JSON object.
            SseEvent::ReasoningDelta { delta } => {
                if verbose {
                    if !in_reasoning {
                        eprint!("[reasoning] ");
                        in_reasoning = true;
                    }
                    eprint!("{delta}");
                }
            }
            // The assistant reply is accumulated (json) or ignored (text, which
            // uses `Finished.content`); it is never streamed to stderr.
            SseEvent::AssistantContentDelta { delta } => {
                end_reasoning(&mut in_reasoning);
                if json {
                    assistant.push_str(&delta);
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
                end_reasoning(&mut in_reasoning);
                if json {
                    assistant.push_str(&content);
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
            SseEvent::Finished { content } => {
                end_reasoning(&mut in_reasoning);
                if !json && !content.is_empty() {
                    print!("{content}");
                }
                // `return` (not `break`) so the post-loop "stream ended"
                // fallback below only runs when no terminal event arrived.
                return Outcome {
                    exit: RunExit::Success,
                    assistant: content,
                };
            }
            SseEvent::Error { message } => {
                end_reasoning(&mut in_reasoning);
                eprintln!("{message}");
                return Outcome {
                    exit: RunExit::Error,
                    assistant,
                };
            }
            SseEvent::MaxRoundsExceeded => {
                end_reasoning(&mut in_reasoning);
                eprintln!("max rounds exceeded");
                return Outcome {
                    exit: RunExit::MaxRounds,
                    assistant,
                };
            }
            SseEvent::Cancelled => {
                end_reasoning(&mut in_reasoning);
                eprintln!("cancelled");
                return Outcome {
                    exit: RunExit::Cancelled,
                    assistant,
                };
            }
            // The round was interrupted; the daemon-side agent stays alive, but this
            // one-shot run will not produce a `Finished`. Treat like Cancelled.
            SseEvent::Interrupted => {
                end_reasoning(&mut in_reasoning);
                eprintln!("interrupted");
                return Outcome {
                    exit: RunExit::Cancelled,
                    assistant,
                };
            }
            SseEvent::TokenBudgetExceeded { consumed, budget } => {
                end_reasoning(&mut in_reasoning);
                eprintln!("token budget exceeded (consumed: {consumed}, budget: {budget})");
                return Outcome {
                    exit: RunExit::BudgetExceeded,
                    assistant,
                };
            }
            SseEvent::FailoverChainExhausted { reason, detail } => {
                end_reasoning(&mut in_reasoning);
                eprintln!("failover chain exhausted ({reason}): {detail}");
                return Outcome {
                    exit: RunExit::FailoverChainExhausted,
                    assistant,
                };
            }
            // Suppress state-transition/informational events.
            SseEvent::Busy | SseEvent::Status { .. } | SseEvent::ApprovalUpdated { .. } => {}
        }
    }

    // The stream ended without a terminal event (daemon crash, network drop).
    end_reasoning(&mut in_reasoning);
    eprintln!("stream ended without a terminal event");

    Outcome { exit, assistant }
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
    async fn json_accumulates_assistant_and_marks_success() {
        let events: Vec<Item> = vec![
            // Reasoning is not accumulated in JSON (only streamed when verbose).
            Ok(SseEvent::ReasoningDelta {
                delta: "thinking".into(),
            }),
            Ok(SseEvent::AssistantContentDelta {
                delta: "Hello".into(),
            }),
            Ok(SseEvent::AssistantContentDelta {
                delta: ", world".into(),
            }),
            Ok(SseEvent::Finished {
                content: "Hello, world".into(),
            }),
        ];
        let outcome = consume_stream(stream::iter(events), true, false).await;
        assert_eq!(outcome.exit, RunExit::Success);
        // Finished.content is authoritative and overwrites delta accumulation.
        assert_eq!(outcome.assistant, "Hello, world");
    }

    #[tokio::test]
    async fn json_keeps_partial_assistant_on_error() {
        let events: Vec<Item> = vec![
            Ok(SseEvent::AssistantContentDelta {
                delta: "partial".into(),
            }),
            Ok(SseEvent::Error {
                message: "boom".into(),
            }),
        ];
        let outcome = consume_stream(stream::iter(events), true, false).await;
        assert_eq!(outcome.exit, RunExit::Error);
        assert_eq!(outcome.assistant, "partial");
    }

    #[tokio::test]
    async fn json_defaults_to_error_without_terminal_event() {
        let events: Vec<Item> = vec![Ok(SseEvent::ReasoningDelta { delta: "x".into() })];
        let outcome = consume_stream(stream::iter(events), true, false).await;
        assert_eq!(outcome.exit, RunExit::Error);
    }

    #[tokio::test]
    async fn terminal_events_map_to_correct_exit() {
        let cases: [(SseEvent, RunExit); 6] = [
            (
                SseEvent::Finished {
                    content: String::new(),
                },
                RunExit::Success,
            ),
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
            let outcome = consume_stream(stream::iter(events), true, false).await;
            assert_eq!(outcome.exit, expected);
        }
    }
}
