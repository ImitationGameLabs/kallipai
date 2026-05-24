mod approval;
mod completion;
mod events;
mod history;
mod input;
mod markdown;
mod render;
mod wrap;

use ratatui_textarea::TextArea;

use approval::ApprovalState;
use completion::CompletionState;

/// A line in the chat output area.
#[derive(Debug)]
pub enum ChatLine {
    User(String),
    Assistant(String),
    ToolCall { name: String, args: String },
    ToolResult(String),
    Reasoning(String),
    Status(String),
    Error(String),
    System(String),
}

/// TUI application state.
pub struct App {
    pub chat_lines: Vec<ChatLine>,
    pub textarea: TextArea<'static>,
    pub auto_scroll: bool,
    pub agent_busy: bool,
    pub should_quit: bool,
    pub kill_on_exit: bool,
    quit_confirm: bool,
    completion: CompletionState,
    approval: ApprovalState,
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
            quit_confirm: false,
            completion: CompletionState::new(),
            approval: ApprovalState::new(),
            history: history::InputHistory::new(),
        }
    }
}
