mod args;
mod command;
mod session;
mod tui;

use anyhow::Result;
use clap::Parser;
use futures_util::StreamExt;
use kallip_client::DaemonClient;
use tokio::sync::mpsc;

use crate::tui::{Outgoing, prepare_outgoing};
use args::Args;
use session::Session;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // TUI mode: write logs to file
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let log_path = std::env::var("KALLIP_DATA_DIR")
        .map(|d| std::path::PathBuf::from(d).join("agent.log"))
        .unwrap_or_else(|_| {
            dirs::data_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("kallip")
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

    let token = match std::env::var("KALLIP_AUTH_TOKEN").ok() {
        Some(tok) => tok,
        None => {
            eprint!("Auth token: ");
            let tok = rpassword::read_password()?;
            let tok = tok.trim().to_owned();
            if tok.is_empty() {
                anyhow::bail!("no token provided");
            }
            tok
        }
    };
    let client = DaemonClient::builder(&args.daemon_url)
        .auth_token(token)
        .build()?;

    let session = Session::connect(client).await?;

    run_tui(session).await
}

/// Fire-and-forget prompt delivery. Results arrive via SSE, with send success
/// or failure reported back through the [`SendOutcome`] feedback channel so the
/// main loop can render an error and re-stash the prompt on failure.
enum Action {
    SendPrompt(Outgoing),
}

/// Outcome of a background `post_message` attempt.
enum SendOutcome {
    Delivered,
    Failed { outgoing: Outgoing, error: String },
}

async fn run_tui(session: Session) -> Result<()> {
    // Subscribe to SSE before anything else.
    let mut event_stream = session.client.event_stream(&session.agent_id).await?;

    // Channel for prompt delivery to the background task. Capacity is small and
    // bounded so a stalled daemon back-pressures into a local `try_send` failure
    // (which re-stashes the prompt) rather than buffering an unbounded burst.
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
    // Feedback channel: background task reports each send outcome so the main
    // loop can surface failures and re-queue the prompt for retry. Outcomes are
    // sent with `blocking_send` — losing a `Failed` would orphan an already-
    // rendered user line, so the sender waits for the main loop rather than
    // dropping on a full channel.
    let (feedback_tx, mut feedback_rx) = mpsc::channel::<SendOutcome>(16);

    // Background task: delivers prompts to the daemon and reports the outcome.
    {
        let client = session.client.clone();
        let agent_id = session.agent_id.clone();
        tokio::spawn(async move {
            while let Some(action) = action_rx.recv().await {
                let Action::SendPrompt(outgoing) = action;
                match client.post_message(&agent_id, &outgoing.payload).await {
                    Ok(_) => {
                        // A dropped `Delivered` during shutdown is harmless.
                        let _ = feedback_tx.try_send(SendOutcome::Delivered);
                    }
                    Err(e) => {
                        // `send().await` waits for capacity rather than dropping:
                        // a lost `Failed` would never re-queue (the outgoing is
                        // owned only here). On shutdown the receiver drops and
                        // this returns `Err`, which we ignore.
                        let _ = feedback_tx
                            .send(SendOutcome::Failed {
                                outgoing,
                                error: e.to_string(),
                            })
                            .await;
                    }
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

    // Chain a panic hook so a panic pops the keyboard enhancement flags and
    // disables mouse capture before ratatui's own hook restores the terminal.
    // Without this, a panic would leak the Kitty keyboard-protocol mode into the
    // user's shell, corrupting key reporting for subsequent programs until reset.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::event::PopKeyboardEnhancementFlags,
            ratatui::crossterm::event::DisableMouseCapture,
        );
        previous_hook(info);
    }));

    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
    )?;
    // Request modifier-augmented key reporting so Shift+Enter (and other modified
    // keys) arrive as distinct events. Required for Shift+Enter to insert a newline
    // instead of submitting. Ignored (no-op) on terminals that lack the protocol.
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::PushKeyboardEnhancementFlags(
            ratatui::crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        )
    )?;
    let mut app = tui::App::new();
    // Filled when the daemon event stream ends; breaks the loop and is surfaced
    // after the terminal is restored. Without this, a dead stream leaves the TUI
    // silently frozen — no events, prompts vanish.
    let mut stream_end: Option<session::StreamEnd> = None;

    loop {
        terminal.draw(|frame| app.render(frame))?;

        tokio::select! {
            Some(event) = key_rx.recv() => {
                match event {
                    ratatui::crossterm::event::Event::Key(key) => {
                        app.handle_key_event(key, &session.client, &session.agent_id).await;
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
            event = event_stream.next() => match event {
                Some(Ok(event)) => app.handle_sse_event(event),
                None => {
                    stream_end = Some(session::StreamEnd::Graceful);
                    break;
                }
                Some(Err(e)) => {
                    stream_end = Some(session::StreamEnd::Failed(e.into()));
                    break;
                }
            },
            Some(outcome) = feedback_rx.recv() => match outcome {
                // Only clear the failure hint once nothing is queued: a `Delivered`
                // for one send must not mask the hint set by a different send's
                // `Failed` that is still waiting in `pending` for retry.
                SendOutcome::Delivered => {
                    if app.pending.is_empty() {
                        app.pending_send_failed = false;
                    }
                }
                SendOutcome::Failed { outgoing, error } => {
                    app.requeue_send(outgoing, error);
                }
            },
            _ = tokio::time::sleep(std::time::Duration::from_millis(33)) => {}
        }

        // Drain any prompt handed off by a flush. The main loop owns delivery so
        // that `App` stays free of I/O: it renders the user line here, applies the
        // token-gated spill, and dispatches the send to the background task.
        //
        // The user line is rendered optimistically, before delivery is confirmed,
        // so it lands in the right position relative to the boundary event. A
        // failed send leaves this line in place and pushes an Error line, then
        // re-renders the same text on retry — an accepted duplicate (the daemon
        // never receives it twice).
        while let Some(merged) = app.outbox.take() {
            let outgoing = prepare_outgoing(&merged);
            let display = outgoing.display.clone();
            app.chat_lines.push(tui::ChatLine::User(display));
            app.auto_scroll = true;
            match action_tx.try_send(Action::SendPrompt(outgoing)) {
                Ok(()) => {}
                Err(err) => {
                    // Channel full or closed: recover the payload and re-stash so
                    // it is retried rather than dropped.
                    let reason = match &err {
                        mpsc::error::TrySendError::Full(_) => "local send channel full",
                        mpsc::error::TrySendError::Closed(_) => "local send channel closed",
                    };
                    let Action::SendPrompt(outgoing) = err.into_inner();
                    app.requeue_send(outgoing, reason.to_owned());
                }
            }
        }
    }

    // Pop enhancement flags and disable mouse capture while still in the alternate
    // screen, before `ratatui::restore()` leaves it. On non-Kitty terminals the pop
    // writes an escape that only the alt buffer discards; leaving the screen first
    // would leak it to the user's shell.
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::PopKeyboardEnhancementFlags,
        ratatui::crossterm::event::DisableMouseCapture,
    )
    .ok();
    ratatui::restore();

    session.cleanup(app.kill_on_exit).await;

    // Resolve the stream end *after* the terminal is restored so the message is
    // not garbled by the alt-screen / raw mode.
    match stream_end {
        Some(end) => Err(end.into_error()),
        None => Ok(()),
    }
}
