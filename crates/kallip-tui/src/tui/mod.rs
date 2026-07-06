mod completion;
mod events;
mod history;
mod input;
mod markdown;
mod prompt;
mod render;
mod wrap;

pub(crate) use prompt::{Outgoing, prepare_outgoing};

use kallip_common::protocol::ApprovalEntry;
use ratatui_textarea::{TextArea, WrapMode};

use completion::CompletionState;

/// A line in the chat output area.
#[derive(Debug)]
pub enum ChatLine {
    User(String),
    Assistant(String),
    ToolCall {
        name: String,
        args: String,
    },
    ToolResult(String),
    Reasoning(String),
    Status(String),
    Error(String),
    System(String),
    Retrying {
        attempt: u32,
        max_attempts: u32,
        error: String,
        delay_secs: f64,
    },
    /// Within-tier failover hop. Non-terminal (the agent stays busy and continues the turn).
    Failover {
        from: String,
        to: String,
        reason: String,
    },
    /// Failover chain exhausted. Terminal for the turn (the agent goes idle but stays alive).
    FailoverExhausted {
        reason: String,
        detail: String,
    },
}

/// Active TUI view.
pub enum AppMode {
    Chat,
    Approvals,
}

/// Phase within the approvals view.
pub enum ApprovalPhase {
    /// Navigating the approvals list.
    Browsing,
    /// Selected an action — showing approve/deny options.
    Deciding,
    /// Typing a deny reason.
    DenyInput { buffer: String },
}

/// State for the approvals view.
pub struct ApprovalsState {
    entries: Vec<ApprovalEntry>,
    selected: usize,
    phase: ApprovalPhase,
    /// Set when an ApprovalUpdated SSE event arrives; triggers re-fetch on next key.
    stale: bool,
}

impl ApprovalsState {
    fn new(entries: Vec<ApprovalEntry>) -> Self {
        Self {
            entries,
            selected: 0,
            phase: ApprovalPhase::Browsing,
            stale: false,
        }
    }
}

/// TUI application state.
pub struct App {
    pub chat_lines: Vec<ChatLine>,
    pub textarea: TextArea<'static>,
    pub auto_scroll: bool,
    pub agent_busy: bool,
    pub should_quit: bool,
    pub kill_on_exit: bool,
    pub mode: AppMode,
    pub approvals: Option<ApprovalsState>,
    quit_confirm: bool,
    completion: CompletionState,
    history: history::InputHistory,
    scroll_pos: usize,
    content_length: usize,
    visible_height: usize,
    streaming_content: bool,
    streaming_reasoning: bool,
    /// Queued user inputs awaiting send. Consecutive entries submitted while the
    /// agent is busy are merged (`join("\n")`) and flushed at the next daemon
    /// interjection boundary; an idle submit flushes immediately as its own turn.
    pub pending: Vec<String>,
    /// Merged prompt handed to the main loop for sending. Single slot: a second
    /// flush before the first is consumed is dropped (see [`App::request_flush`]).
    pub outbox: Option<String>,
    /// Set when the last send failed; surfaced in the input title as a hint.
    pub pending_send_failed: bool,
}

impl App {
    /// The bordered block for the input area, with the given title. Shared
    /// between [`App::new`] and the per-frame title refresh so the border style
    /// has a single source of truth.
    fn input_block(title: String) -> ratatui::widgets::Block<'static> {
        ratatui::widgets::Block::bordered()
            .title(title)
            .border_style(ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray))
    }

    pub fn new() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_block(Self::input_block(">> ".into()));
        textarea.set_placeholder_text("Type a message...");
        // Wrap long input at word boundaries, falling back to glyph splitting for a
        // single token wider than the viewport (e.g. a pasted URL). Without this,
        // the textarea scrolls horizontally and stretches a single row to the right.
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        // Drop the default underline on the cursor line: it underlines every
        // character on the active line and leaves underline residue after deletion.
        // The caret itself (reversed block) stays visible via `cursor_style`.
        textarea.set_cursor_line_style(ratatui::style::Style::default());
        Self {
            chat_lines: Vec::new(),
            textarea,
            scroll_pos: 0,
            content_length: 0,
            visible_height: 0,
            streaming_content: false,
            streaming_reasoning: false,
            auto_scroll: true,
            agent_busy: false,
            should_quit: false,
            kill_on_exit: false,
            mode: AppMode::Chat,
            approvals: None,
            quit_confirm: false,
            completion: CompletionState::new(),
            history: history::InputHistory::new(),
            pending: Vec::new(),
            outbox: None,
            pending_send_failed: false,
        }
    }

    /// Move queued user input into the outbox so the main loop can send it.
    ///
    /// Called on idle submit and on daemon interjection boundaries (a completed
    /// assistant message, or a terminal event). No-op when nothing is pending or
    /// when a previous flush is still waiting to be consumed — the outbox is a
    /// single slot, so a second flush would silently overwrite the first.
    pub fn request_flush(&mut self) {
        if self.outbox.is_some() {
            tracing::warn!("request_flush: outbox occupied, skipping to avoid drop");
            return;
        }
        if self.pending.is_empty() {
            return;
        }
        let merged = self.pending.join("\n");
        self.pending.clear();
        self.outbox = Some(merged);
    }

    /// Re-stash a failed send for retry, preserving the original text.
    ///
    /// `outgoing.raw` (not the payload/spill-instruction) goes back to the front
    /// of `pending`, so a retry re-evaluates the spill from scratch and never
    /// leaks a temp-file path into the merged prompt.
    pub fn requeue_send(&mut self, outgoing: Outgoing, error: String) {
        self.pending.insert(0, outgoing.raw);
        self.pending_send_failed = true;
        self.chat_lines
            .push(ChatLine::Error(format!("send failed, will retry: {error}")));
        self.auto_scroll = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_flush_merges_and_clears() {
        let mut app = App::new();
        app.pending.push("a".into());
        app.pending.push("b".into());
        app.request_flush();
        assert_eq!(app.outbox.as_deref(), Some("a\nb"));
        assert!(app.pending.is_empty());
    }

    #[test]
    fn request_flush_noop_when_empty() {
        let mut app = App::new();
        app.request_flush();
        assert!(app.outbox.is_none());
    }

    #[test]
    fn request_flush_does_not_overwrite_occupied_outbox() {
        // Single-slot outbox: a second flush before the first is consumed must
        // not drop the earlier message.
        let mut app = App::new();
        app.pending.push("first".into());
        app.request_flush();
        app.pending.push("second".into());
        app.request_flush();
        assert_eq!(app.outbox.as_deref(), Some("first"));
        assert_eq!(app.pending, vec!["second".to_string()]);
    }

    #[test]
    fn requeue_send_stashes_raw_not_payload() {
        // `requeue_send` must stash `raw`, never `payload`. Construct a spilled
        // Outgoing directly so the test does not depend on the token threshold
        // (the spill heuristic is covered separately in `prompt::tests`).
        let mut app = App::new();
        let raw = "the original user text".to_owned();
        let instruction = "read /tmp/spilled ...".to_owned();
        let outgoing = Outgoing {
            raw: raw.clone(),
            display: instruction.clone(),
            payload: instruction.clone(),
        };
        app.requeue_send(outgoing, "boom".into());

        assert_eq!(app.pending, vec![raw], "raw re-stashed, not payload");
        assert!(app.pending_send_failed);
        assert_ne!(
            app.pending[0], instruction,
            "the spill instruction must not leak into pending"
        );
        match app.chat_lines.last() {
            Some(ChatLine::Error(_)) => {}
            other => panic!("expected an error chat line, got {other:?}"),
        }
    }
}
