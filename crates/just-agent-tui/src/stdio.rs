//! Stdio-based interactive chat mode — a lightweight alternative to the TUI.

use std::io::{self, Write};

use anyhow::Result;
use futures_util::StreamExt;
use just_agent_common::command::{self, SlashCommand};
use just_agent_common::types::SseEvent;
use tokio::sync::mpsc;

use crate::session::Session;

/// Pending interactive prompt waiting for the next stdin line.
enum PendingPrompt {
    None,
    QuitConfirm,
}

enum Action {
    SendPrompt(String),
}

/// Action resulting from a stdin line, to be dispatched by the main loop.
enum StdinAction {
    None,
    SendPrompt(String),
    Quit { kill: bool },
    Command(SlashCommand),
}

pub async fn run_stdio(session: Session) -> Result<()> {
    let mut event_stream = session.client.event_stream(&session.agent_id).await?;

    let (action_tx, mut action_rx) = mpsc::channel::<Action>(64);

    // Background task: drain async actions.
    {
        let client = session.client.clone();
        let agent_id = session.agent_id.clone();
        tokio::spawn(async move {
            while let Some(action) = action_rx.recv().await {
                let result = match action {
                    Action::SendPrompt(text) => client.post_message(&agent_id, &text).await,
                };
                if let Err(e) = result {
                    eprintln!("[error] {e}");
                }
            }
        });
    }

    // Stdin line reader — runs on a plain OS thread to avoid blocking
    // the tokio runtime with synchronous stdin reads.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(64);
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut buf = String::new();
        loop {
            buf.clear();
            match stdin.read_line(&mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let line = buf.trim_end().to_owned();
                    if stdin_tx.blocking_send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut busy = false;
    let mut should_quit = false;
    let mut kill_on_exit = false;
    let mut pending = PendingPrompt::None;

    loop {
        if should_quit {
            break;
        }

        if !busy && matches!(pending, PendingPrompt::None) {
            print!("You> ");
            io::stdout().flush().ok();
        }

        tokio::select! {
            // Stdin input
            Some(line) = stdin_rx.recv() => {
                if line.is_empty() {
                    continue;
                }

                match handle_stdin_line(&line, &mut pending) {
                    StdinAction::None => {}
                    StdinAction::SendPrompt(text) => {
                        println!();
                        busy = true;
                        action_tx.send(Action::SendPrompt(text)).await.ok();
                    }
                    StdinAction::Quit { kill } => {
                        kill_on_exit = kill;
                        should_quit = true;
                    }
                    StdinAction::Command(cmd) => {
                        handle_command(cmd, &session, &mut pending).await;
                    }
                }
            }

            // SSE events
            Some(result) = event_stream.next() => {
                match result {
                    Ok(event) => {
                        handle_sse_event(event, &mut busy);
                    }
                    Err(e) => {
                        eprintln!("[error] SSE: {e}");
                    }
                }
            }

            // Background action results are logged inside the spawned task
            else => break,
        }
    }

    session.cleanup(kill_on_exit).await;

    Ok(())
}

fn handle_sse_event(event: SseEvent, busy: &mut bool) {
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
        SseEvent::DeferredActionUpdated { id, status } => {
            println!("[approval] {id} {status}");
        }
        SseEvent::Retrying {
            attempt,
            max_attempts,
            error,
            delay_secs,
        } => {
            eprintln!("[retry {attempt}/{max_attempts}] {error} — waiting {delay_secs:.1}s");
        }
        SseEvent::Cancelled => {
            eprintln!("[cancelled]");
            *busy = false;
        }
    }
}

async fn handle_command(cmd: SlashCommand, session: &Session, pending: &mut PendingPrompt) {
    match cmd {
        SlashCommand::Help => {
            print!("{}", command::help_text());
        }
        SlashCommand::Quit => {
            eprint!("[quit] [1] Keep agent running and quit  [2] Stop agent and quit: ");
            io::stderr().flush().ok();
            *pending = PendingPrompt::QuitConfirm;
        }
        SlashCommand::Clear => {}
        SlashCommand::Status => match session.client.agent_status(&session.agent_id).await {
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
        SlashCommand::Approvals => {
            eprintln!("[system] /approvals is only available in TUI mode");
        }
    }
}

/// Format a JSON value for display in stdio deferred action prompts.
/// Objects and arrays are pretty-printed; scalars use compact form.
fn handle_stdin_line(line: &str, pending: &mut PendingPrompt) -> StdinAction {
    let trimmed = line.trim();
    match pending {
        PendingPrompt::QuitConfirm => match trimmed {
            "1" => {
                *pending = PendingPrompt::None;
                StdinAction::Quit { kill: false }
            }
            "2" => {
                *pending = PendingPrompt::None;
                StdinAction::Quit { kill: true }
            }
            _ => {
                eprint!("[quit] [1] Keep agent running and quit  [2] Stop agent and quit: ");
                io::stderr().flush().ok();
                StdinAction::None
            }
        },
        PendingPrompt::None => match command::parse(line) {
            None => StdinAction::SendPrompt(line.to_owned()),
            Some(Ok(cmd)) => StdinAction::Command(cmd),
            Some(Err(msg)) => {
                eprintln!("[error] {msg}");
                StdinAction::None
            }
        },
    }
}
