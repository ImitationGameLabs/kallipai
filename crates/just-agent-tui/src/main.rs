mod args;
mod stdio;
mod tui;

use anyhow::Result;
use clap::Parser;
use futures_util::StreamExt;
use just_agent_client::DaemonClient;
use just_agent_core::types::AgentId;
use tokio::sync::mpsc;

use args::Args;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // TUI mode: write logs to file
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let log_path = std::env::var("JUST_AGENT_DATA_DIR")
        .map(|d| std::path::PathBuf::from(d).join("agent.log"))
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("just-agent")
                .join("agent.log")
        });
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(file) = std::fs::File::create(&log_path) {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(file)
            .with_ansi(false)
            .init();
    }

    let token = match std::env::var("JUST_AGENT_AUTH_TOKEN").ok() {
        Some(tok) => tok,
        None if !args.stdio => {
            eprint!("Auth token (from daemon output): ");
            let mut tok = String::new();
            std::io::stdin().read_line(&mut tok)?;
            let tok = tok.trim().to_owned();
            if tok.is_empty() {
                anyhow::bail!("no token provided");
            }
            tok
        }
        None => {
            anyhow::bail!(
                "JUST_AGENT_AUTH_TOKEN not set. Set the env var or run without --stdio for interactive prompt."
            );
        }
    };
    let client = DaemonClient::new_with_token(&args.daemon_url, token);
    let agent_id = client
        .spawn(just_agent_core::types::CreateAgentRequest {
            workspace_root: None,
            skills: args.skills,
            prompt: None,
            created_by: None,
        })
        .await?;

    if args.stdio {
        stdio::run_stdio(client, agent_id).await
    } else {
        run_tui(client, agent_id).await
    }
}

/// Fire-and-forget prompt delivery. Results arrive via SSE.
enum Action {
    SendPrompt(String),
}

async fn run_tui(client: DaemonClient, agent_id: AgentId) -> Result<()> {
    // Subscribe to SSE before anything else.
    let mut event_stream = client.event_stream(&agent_id).await?;

    // Channel for prompt delivery to the background task.
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(64);

    // Background task: delivers prompts to the daemon.
    {
        let client = client.clone();
        let agent_id = agent_id.clone();
        tokio::spawn(async move {
            while let Some(action) = action_rx.recv().await {
                let Action::SendPrompt(text) = action;
                if let Err(e) = client.post_message(&agent_id, &text).await {
                    tracing::error!("prompt delivery failed: {e}");
                }
            }
        });
    }

    // Crossterm input thread.
    let (key_tx, mut key_rx) = mpsc::channel::<ratatui::crossterm::event::Event>(64);
    std::thread::spawn(move || {
        while let Ok(event) = ratatui::crossterm::event::read() {
            if key_tx.blocking_send(event).is_err() {
                break;
            }
        }
    });

    ratatui::try_init()?;
    let mut terminal = ratatui::init();
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
    )?;
    let mut app = tui::App::new();

    loop {
        terminal.draw(|frame| app.render(frame))?;

        tokio::select! {
            Some(event) = key_rx.recv() => {
                match event {
                    ratatui::crossterm::event::Event::Key(key) => {
                        app.handle_key_event(key, &action_tx, &client, &agent_id).await;
                        if app.should_quit {
                            break;
                        }
                    }
                    ratatui::crossterm::event::Event::Mouse(mouse) => {
                        let chat_height = terminal.get_frame().area().height.saturating_sub(7);
                        app.handle_mouse_event(mouse, chat_height);
                    }
                    _ => {}
                }
            }
            Some(result) = event_stream.next() => {
                match result {
                    Ok(event) => app.handle_sse_event(event),
                    Err(e) => {
                        app.push_error(format!("SSE error: {e}"));
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(33)) => {}
        }
    }

    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableMouseCapture
    )
    .ok();
    ratatui::restore();

    if app.kill_on_exit
        && let Err(e) = client.kill_agent(&agent_id).await
    {
        tracing::warn!("failed to kill agent on exit: {e}");
    }

    Ok(())
}
