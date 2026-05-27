//! Stdio-based interactive chat mode — a lightweight alternative to the TUI.

use std::io::{self, Write};

use anyhow::Result;
use futures_util::StreamExt;
use just_agent_client::DaemonClient;
use just_agent_core::command::{self, SlashCommand};
use just_agent_core::types::SseEvent;
use tokio::sync::mpsc;

pub async fn run_stdio(client: DaemonClient, agent_id: String) -> Result<()> {
    let mut event_stream = client.event_stream(&agent_id).await?;

    let (action_tx, mut action_rx) = mpsc::channel::<Action>(64);

    // Background task: drain async actions.
    {
        let client = client.clone();
        let agent_id = agent_id.clone();
        tokio::spawn(async move {
            while let Some(action) = action_rx.recv().await {
                let result = match action {
                    Action::SendPrompt(text) => client.post_message(&agent_id, &text).await,
                    Action::RespondApproval { request_id, decision } => {
                        client
                            .respond_approval(&agent_id, &request_id, &decision, None)
                            .await
                    }
                };
                if let Err(e) = result {
                    eprintln!("[error] {e}");
                }
            }
        });
    }

    // Channel for approval requests forwarded from SSE handling.
    let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalPrompt>(4);

    // Stdin line reader — a background task that reads lines and sends them.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(64);
    tokio::spawn(async move {
        let stdin = io::stdin();
        let mut buf = String::new();
        loop {
            buf.clear();
            match stdin.read_line(&mut buf) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let line = buf.trim_end().to_owned();
                    if stdin_tx.send(line).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut busy = false;
    let mut should_quit = false;

    loop {
        if should_quit {
            break;
        }

        if !busy {
            print!("You> ");
            io::stdout().flush().ok();
        }

        tokio::select! {
            // Stdin input
            Some(line) = stdin_rx.recv() => {
                if line.is_empty() {
                    continue;
                }

                match command::parse(&line) {
                    None => {
                        println!();
                        busy = true;
                        action_tx.send(Action::SendPrompt(line)).await.ok();
                    }
                    Some(Ok(cmd)) => {
                        handle_command(cmd, &client, &agent_id, &mut should_quit).await;
                    }
                    Some(Err(msg)) => {
                        eprintln!("[error] {msg}");
                    }
                }
            }

            // SSE events
            Some(result) = event_stream.next() => {
                match result {
                    Ok(event) => {
                        handle_sse_event(event, &approval_tx, &mut busy);
                    }
                    Err(e) => {
                        eprintln!("[error] SSE: {e}");
                    }
                }
            }

            // Approval prompt forwarded from SSE
            Some(prompt) = approval_rx.recv() => {
                handle_approval(&prompt, &action_tx).await;
            }

            // Background action results are logged inside the spawned task
            else => break,
        }
    }

    // Clean up: kill the agent on exit.
    if let Err(e) = client.kill_agent(&agent_id).await {
        tracing::warn!("failed to kill agent on exit: {e}");
    }

    Ok(())
}

enum Action {
    SendPrompt(String),
    RespondApproval { request_id: String, decision: String },
}

struct ApprovalPrompt {
    request_id: String,
    tool_name: String,
    summary: String,
    reason: String,
    dangerous: bool,
}

fn handle_sse_event(event: SseEvent, approval_tx: &mpsc::Sender<ApprovalPrompt>, busy: &mut bool) {
    match event {
        SseEvent::Reasoning { content } => {
            println!("[think] {content}");
        }
        SseEvent::AssistantContent { content } => {
            println!("{content}");
        }
        // Delta events are intentionally ignored in stdio mode — only the
        // final `Finished` content is printed, since incremental output
        // makes no sense in a piped/pipe-friendly interactive session.
        SseEvent::AssistantContentDelta { .. } => {}
        SseEvent::ReasoningDelta { .. } => {}
        SseEvent::ToolCall { name, args } => {
            println!("[tool] {name}({args})");
        }
        SseEvent::ToolResult { result } => {
            println!("[result] {result}");
        }
        SseEvent::Finished { content } => {
            if !content.is_empty() {
                println!("{content}");
            }
            *busy = false;
        }
        SseEvent::MaxRoundsExceeded => {
            eprintln!("[error] max rounds exceeded");
            *busy = false;
        }
        SseEvent::Error { message } => {
            eprintln!("[error] {message}");
            *busy = false;
        }
        SseEvent::Status { message } => {
            println!("{message}");
        }
        SseEvent::Busy => {
            *busy = true;
        }
        SseEvent::DeferredCreated { request_id, tool_name, summary, reason, dangerous } => {
            approval_tx
                .try_send(ApprovalPrompt { request_id, tool_name, summary, reason, dangerous })
                .ok();
        }
        SseEvent::DeferredApproved { request_id } => {
            println!("[deferred] {request_id} approved");
        }
        SseEvent::DeferredDenied { request_id, reason } => {
            eprintln!("[deferred] {request_id} denied: {reason}");
        }
        SseEvent::Retrying { attempt, max_attempts, error, delay_secs } => {
            eprintln!("[retry {attempt}/{max_attempts}] {error} — waiting {delay_secs:.1}s");
        }
        SseEvent::Cancelled => {
            eprintln!("[cancelled]");
            *busy = false;
        }
    }
}

async fn handle_command(
    cmd: SlashCommand,
    client: &DaemonClient,
    agent_id: &str,
    should_quit: &mut bool,
) {
    match cmd {
        SlashCommand::Help => {
            print!("{}", command::help_text());
        }
        SlashCommand::Quit => {
            *should_quit = true;
        }
        SlashCommand::Clear => {
            // No buffer to clear in stdio mode.
        }
        SlashCommand::Status => match client.agent_status(agent_id).await {
            Ok(status) => {
                println!("{}", status.context.format_summary());
                if !status.recent_retries.is_empty() {
                    println!(
                        "retries: {} (last: {})",
                        status.recent_retries.len(),
                        status
                            .recent_retries
                            .first()
                            .map(|r| r.error.as_str())
                            .unwrap_or("n/a")
                    );
                }
            }
            Err(e) => eprintln!("[error] {e}"),
        },
    }
}

async fn handle_approval(prompt: &ApprovalPrompt, action_tx: &mpsc::Sender<Action>) {
    let label = if prompt.dangerous { "DANGER" } else { "approval" };
    eprintln!("[{label}] tool: {}", prompt.tool_name);
    eprintln!("[{label}] reason: {}", prompt.reason);
    eprintln!("[{label}] cmd: {}", prompt.summary);

    eprint!("[{label}] [1] Approve  [2] Deny: ");
    io::stderr().flush().ok();

    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();

    let decision = match input.trim() {
        "1" => "approve",
        _ => "deny",
    };
    action_tx
        .send(Action::RespondApproval {
            request_id: prompt.request_id.clone(),
            decision: decision.to_owned(),
        })
        .await
        .ok();
}
