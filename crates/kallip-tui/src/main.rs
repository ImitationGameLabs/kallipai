mod args;
mod command;
mod session;
mod tui;

use anyhow::Result;
use clap::Parser;
use futures_util::StreamExt;
use kallip_client::TagmaClient;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::tui::{DisableAlternateScroll, EnableAlternateScroll, Outgoing, prepare_outgoing};
use args::Args;
use session::Session;

/// Frame-rate cap for streaming redraws (~60 fps). Only the high-frequency
/// case (content/reasoning deltas) is coalesced to this interval; state
/// mutation stays immediate and all other events redraw at once.
const STREAM_FRAME_INTERVAL: Duration = Duration::from_millis(16);

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
    let client = TagmaClient::builder(&args.tagma_url)
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
    // bounded so a stalled tagma back-pressures into a local `try_send` failure
    // (which re-stashes the prompt) rather than buffering an unbounded burst.
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(8);
    // Feedback channel: background task reports each send outcome so the main
    // loop can surface failures and re-queue the prompt for retry. Outcomes are
    // sent with `blocking_send` — losing a `Failed` would orphan an already-
    // rendered user line, so the sender waits for the main loop rather than
    // dropping on a full channel.
    let (feedback_tx, mut feedback_rx) = mpsc::channel::<SendOutcome>(16);

    // Background task: delivers prompts to the tagma and reports the outcome.
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

    // Chain a panic hook so a panic pops the keyboard enhancement flags and disables
    // alternate-scroll before ratatui's own hook restores the terminal. Both escapes
    // only apply inside the alt screen, so they must be written while still in it;
    // without this, a panic would leak the Kitty keyboard-protocol mode into the
    // user's shell, corrupting key reporting for subsequent programs until reset.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            ratatui::crossterm::event::PopKeyboardEnhancementFlags,
            ratatui::crossterm::event::DisableBracketedPaste,
            DisableAlternateScroll,
        );
        previous_hook(info);
    }));

    // Request modifier-augmented key reporting so Shift+Enter (and other modified
    // keys) arrive as distinct events. Required for Shift+Enter to insert a newline
    // instead of submitting. Ignored (no-op) on terminals that lack the protocol.
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::PushKeyboardEnhancementFlags(
            ratatui::crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        )
    )?;

    // Enable bracketed paste so a multi-character paste arrives as a single
    // `Event::Paste(String)` (inserted in one batched edit) instead of a burst of
    // one-char `KeyEvent`s that render char-by-char. Terminals without the mode
    // keep the per-char fallback. Disabled alongside the other alt-screen escapes
    // in the panic hook and the exit block below.
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableBracketedPaste
    )?;

    // Mouse capture stays OFF so the terminal's native click-drag text selection
    // works everywhere. Instead, enable alternate-scroll: the terminal translates the
    // mouse wheel into Up/Down arrows, which the input handler binds to chat scrolling
    // (see `tui::input`). This keeps both wheel scrolling and native selection.
    ratatui::crossterm::execute!(std::io::stdout(), EnableAlternateScroll)?;
    let mut app = tui::App::new();
    // Filled when the tagma event stream ends; breaks the loop and is surfaced
    // after the terminal is restored. Without this, a dead stream leaves the TUI
    // silently frozen — no events, prompts vanish.
    let mut stream_end: Option<session::StreamEnd> = None;

    // Frame-rate cap for streaming redraws. State mutation stays immediate
    // (every delta is appended at once); only the redraw is coalesced, so the
    // final state is always correct. Idle CPU stays at zero: `redraw_scheduled`
    // only arms the timer branch while a deferred delta is pending, and select!
    // parks the task when nothing is ready.
    let mut last_draw = Instant::now();
    let mut redraw_scheduled = false;

    loop {
        // Event-driven redraw: draw only when a handler signaled a change. With no
        // tick, idle CPU stays at zero; the only periodic work is the SSE stream
        // itself. `Event::Resize` is handled below so a resize still redraws.
        // Clearing `redraw_scheduled` here (not at timer-fire) guarantees a
        // deferred delta is never left undrawn until some unrelated event wakes
        // the loop.
        if app.take_dirty() {
            terminal.draw(|frame| app.render(frame))?;
            last_draw = Instant::now();
            redraw_scheduled = false;
        }

        tokio::select! {
            biased;
            Some(event) = key_rx.recv() => {
                match event {
                    ratatui::crossterm::event::Event::Key(key) => {
                        app.handle_key_event(key, &session.client, &session.agent_id).await;
                        app.mark_dirty();
                        if app.should_quit {
                            break;
                        }
                    }
                    // A bracketed-paste payload arrives as one event; insert it as
                    // a single batched edit (see `App::apply_bracketed_paste`).
                    ratatui::crossterm::event::Event::Paste(text) => {
                        app.apply_bracketed_paste(text).await;
                        app.mark_dirty();
                    }
                    // crossterm multiplexes Resize through the same event channel;
                    // without the old 30Hz tick this is the only resize signal, so
                    // it must trigger a redraw.
                    ratatui::crossterm::event::Event::Resize(..) => {
                        app.mark_dirty();
                    }
                    _ => {}
                }
            }
            event = event_stream.next() => match event {
                Some(Ok(event)) => {
                    // Coalesce only the high-frequency case (content/reasoning
                    // deltas); boundaries, tool events, and errors redraw
                    // promptly. The deferred flush is bounded to one frame by the
                    // timer branch below.
                    let is_delta = app.handle_sse_event(event);
                    if is_delta && last_draw.elapsed() < STREAM_FRAME_INTERVAL {
                        redraw_scheduled = true;
                    } else {
                        app.mark_dirty();
                    }
                }
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
                    app.mark_dirty();
                }
                SendOutcome::Failed { outgoing, error } => {
                    app.requeue_send(outgoing, error);
                    app.mark_dirty();
                }
            },
            // Deferred streaming-redraw flush. The guard disables the branch
            // entirely while no delta is pending, so select! parks the task and
            // idle CPU stays zero. Placed last under `biased` so key/SSE/feedback
            // always win when ready.
            _ = tokio::time::sleep(STREAM_FRAME_INTERVAL.saturating_sub(last_draw.elapsed())), if redraw_scheduled => {
                app.mark_dirty();
            }
        }

        // Drain any prompt handed off by a flush. The main loop owns delivery so
        // that `App` stays free of I/O: it renders the user line here, applies the
        // token-gated spill, and dispatches the send to the background task.
        //
        // The user line is rendered optimistically, before delivery is confirmed,
        // so it lands in the right position relative to the boundary event. A
        // failed send leaves this line in place and pushes an Error line, then
        // re-renders the same text on retry — an accepted duplicate (the tagma
        // never receives it twice).
        while let Some(merged) = app.outbox.take() {
            let outgoing = prepare_outgoing(&merged);
            let display = outgoing.display.clone();
            app.chat_lines.push(tui::ChatLine::User(display));
            app.auto_scroll = true;
            app.mark_dirty();
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

    // Flush any deferred-but-undrawn state (e.g. the final streaming delta when
    // the stream closed before the frame timer fired) before tearing down the alt
    // screen. `take_dirty` runs only at the loop top, which the break skipped.
    if app.take_dirty() {
        terminal.draw(|frame| app.render(frame)).ok();
    }

    // Pop enhancement flags and disable alternate-scroll while still in the alternate
    // screen, before `ratatui::restore()` leaves it. On non-Kitty terminals the pop
    // writes an escape that only the alt buffer discards; leaving the screen first
    // would leak it to the user's shell.
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::PopKeyboardEnhancementFlags,
        ratatui::crossterm::event::DisableBracketedPaste,
        DisableAlternateScroll,
    )
    .ok();
    ratatui::restore();

    // Resolve the stream end *after* the terminal is restored so the message is
    // not garbled by the alt-screen / raw mode.
    match stream_end {
        Some(end) => Err(end.into_error()),
        None => Ok(()),
    }
}
