mod completion;
mod events;
mod history;
mod input;
mod markdown;
mod render;
mod wrap;

use just_agent_common::protocol::ApprovalEntry;
use ratatui_textarea::TextArea;

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
}

impl App {
    pub fn new() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_block(
            ratatui::widgets::Block::bordered()
                .title(">> ")
                .border_style(ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray)),
        );
        textarea.set_placeholder_text("Type a message...");
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
        }
    }
}
