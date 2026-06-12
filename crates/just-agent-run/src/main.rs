//! just-agent-run: agent runner for scripting and benchmarking.
//!
//! Non-interactive CLI that creates an agent via the daemon, streams progress
//! to stderr, prints the final result to stdout, and exits with a semantic
//! exit code. Designed for scripted and automated workflows where the caller
//! needs machine-readable output and exit-status-driven control flow.
//!
//! By default the agent is **preserved** after completion so that logs,
//! history, and token usage remain available for auditing. Pass `--delete`
//! to remove the agent and all associated data after the run finishes.

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use futures_util::StreamExt;
use just_agent_client::DaemonClient;
use just_agent_common::agentid::AgentId;
use just_agent_common::protocol::{CreateAgentRequest, MaxToolRounds, SseEvent};

#[derive(Parser)]
#[command(
    name = "just-agent-run",
    about = "Create an agent, run it to completion, and print the result"
)]
struct Cli {
    /// The prompt to send to the agent.
    prompt: String,
    /// Working directory for the agent.
    #[arg(long)]
    workspace_root: Option<String>,
    /// Maximum tool-call rounds for this agent run.
    /// Overrides the daemon default (unlimited unless JUST_AGENT_MAX_TOOL_ROUNDS is set).
    #[arg(long)]
    max_rounds: Option<usize>,
    /// Delete the agent (including logs, history, and token usage) after completion.
    /// By default the agent is preserved for auditing and can be deleted later
    /// with `just-agent stop <ID>`.
    #[arg(long)]
    delete: bool,
}

/// Semantic exit codes for `just-agent-run`.
///
/// Mapped to process exit codes via `#[repr(u8)]`:
/// 0 = success, 1 = error, 2 = max rounds exceeded,
/// 3 = cancelled, 4 = token budget exceeded.
#[derive(Clone, Copy)]
#[repr(u8)]
enum RunExit {
    Success = 0,
    Error = 1,
    MaxRounds = 2,
    Cancelled = 3,
    BudgetExceeded = 4,
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

    let id = client
        .spawn(CreateAgentRequest {
            workspace_root: cli.workspace_root,
            skills: vec![],
            prompt: Some(cli.prompt),
            created_by: std::env::var("JUST_AGENT_ID").ok().map(AgentId::from),
            max_tool_rounds: cli.max_rounds.map(MaxToolRounds::Limited),
        })
        .await?;

    let exit = consume_stream(&client, &id).await;

    if cli.delete {
        // Delete the agent and all associated data.
        if let Err(e) = client.stop_agent(&id).await {
            eprintln!("warning: failed to delete agent {id}: {e}");
        }
    } else {
        eprintln!("agent {id} finished (kept). Use `just-agent stop {id}` to delete.");
    }

    Ok(exit.into())
}

/// End the current reasoning block, printing a trailing newline if one was
/// active.
fn end_reasoning(in_reasoning: &mut bool) {
    if *in_reasoning {
        eprintln!();
        *in_reasoning = false;
    }
}

/// Subscribe to the agent's SSE stream and print events until a terminal
/// event arrives.
///
/// Returns the exit status. Defaults to [`RunExit::Error`] if the stream
/// closes without a terminal event (daemon crash, network drop).
async fn consume_stream(client: &DaemonClient, id: &AgentId) -> RunExit {
    let mut stream = match client.event_stream(id).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to subscribe to agent events: {e}");
            return RunExit::Error;
        }
    };

    // Default to error — only the Finished arm sets success.
    // If the stream closes without a terminal event, we correctly report failure.
    let mut exit = RunExit::Error;

    let mut in_reasoning = false;
    let mut streamed_to_stderr = false;

    // In terminal mode, stream content deltas to stderr for live feedback and
    // suppress the final print to stdout. When piped, suppress deltas and emit
    // only the full result on stdout for clean capture.
    let stdout_is_terminal = std::io::stdout().is_terminal();

    while let Some(result) = stream.next().await {
        let event = match result {
            Ok(e) => e,
            Err(e) => {
                eprintln!("SSE error: {e}");
                return RunExit::Error;
            }
        };
        match event {
            SseEvent::AssistantContentDelta { delta } => {
                end_reasoning(&mut in_reasoning);
                if stdout_is_terminal {
                    eprint!("{delta}");
                    streamed_to_stderr = true;
                }
            }
            SseEvent::ReasoningDelta { delta } => {
                if !in_reasoning {
                    eprint!("[reasoning] ");
                    in_reasoning = true;
                }
                eprint!("{delta}");
            }
            SseEvent::ToolCall { name, .. } => {
                end_reasoning(&mut in_reasoning);
                eprintln!("[tool] {name}");
            }
            SseEvent::ToolResult { result } => {
                end_reasoning(&mut in_reasoning);
                eprintln!("[tool-result] {result}");
            }
            SseEvent::Retrying {
                attempt,
                max_attempts,
                error,
                delay_secs,
            } => {
                end_reasoning(&mut in_reasoning);
                eprintln!("[retry {attempt}/{max_attempts}] {error} (waiting {delay_secs:.1}s)");
            }
            SseEvent::Finished { content } => {
                end_reasoning(&mut in_reasoning);
                if stdout_is_terminal && streamed_to_stderr {
                    // Content already streamed to stderr; ensure clean terminal line.
                    eprintln!();
                } else if !content.is_empty() {
                    // No streaming happened (piped or no deltas): print to stdout.
                    print!("{content}");
                }
                exit = RunExit::Success;
                break;
            }
            SseEvent::Error { message } => {
                end_reasoning(&mut in_reasoning);
                eprintln!("{message}");
                return RunExit::Error;
            }
            SseEvent::MaxRoundsExceeded => {
                end_reasoning(&mut in_reasoning);
                eprintln!("max rounds exceeded");
                return RunExit::MaxRounds;
            }
            SseEvent::Cancelled => {
                end_reasoning(&mut in_reasoning);
                eprintln!("cancelled");
                return RunExit::Cancelled;
            }
            SseEvent::TokenBudgetExceeded { consumed, budget } => {
                end_reasoning(&mut in_reasoning);
                eprintln!("token budget exceeded (consumed: {consumed}, budget: {budget})");
                return RunExit::BudgetExceeded;
            }
            // Suppress verbose events not needed for scripted usage.
            SseEvent::Busy
            | SseEvent::Status { .. }
            | SseEvent::ApprovalUpdated { .. }
            | SseEvent::AssistantContent { .. }
            | SseEvent::Reasoning { .. } => {}
        }
    }

    exit
}
