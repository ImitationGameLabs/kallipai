use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin};
use ratatui::style::{Color, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};

use super::wrap::word_wrap_line_count;
use super::{App, ChatLine};

impl App {
    /// Render the TUI.
    pub fn render(&mut self, frame: &mut Frame) {
        let [chat_area, input_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(5)]).areas(frame.area());

        let auto_scroll = self.auto_scroll;
        let old_pos = self.scroll_pos;

        let t0 = std::time::Instant::now();
        let text = self.build_chat_text(chat_area.width);
        let build_ms = t0.elapsed().as_millis();

        let content_width = chat_area.width.saturating_sub(2) as usize;
        let visible_height = chat_area.height.saturating_sub(2) as usize;
        let t1 = std::time::Instant::now();
        let total = word_wrap_line_count(&text, content_width);
        let wrap_ms = t1.elapsed().as_millis();

        if build_ms + wrap_ms > 3 {
            tracing::warn!(
                "render: build={}ms wrap={}ms lines={}",
                build_ms,
                wrap_ms,
                total
            );
        }

        let pos = if auto_scroll {
            total.saturating_sub(visible_height)
        } else {
            old_pos.min(total.saturating_sub(visible_height))
        };

        let paragraph = Paragraph::new(text)
            .block(Block::bordered().title("Chat"))
            .wrap(Wrap { trim: true })
            .scroll((pos as u16, 0));
        frame.render_widget(paragraph, chat_area);

        // Scrollbar, only when content overflows viewport.
        let scroll_range = total.saturating_sub(visible_height);
        if scroll_range > 0 {
            let mut scrollbar_state = ScrollbarState::new(scroll_range + 1)
                .position(pos)
                .viewport_content_length(visible_height);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                chat_area.inner(Margin { vertical: 1, horizontal: 0 }),
                &mut scrollbar_state,
            );
        }

        self.scroll_pos = pos;
        self.content_length = total;
        self.visible_height = visible_height;

        self.completion.render(frame, input_area);
        self.approval.render(frame, input_area);
        frame.render_widget(&self.textarea, input_area);
    }

    /// Build styled Text from chat_lines for rendering.
    fn build_chat_text(&self, term_width: u16) -> Text<'_> {
        let mut lines: Vec<Line> = Vec::new();
        for entry in &self.chat_lines {
            match entry {
                ChatLine::User(text) => {
                    lines.push(Line::from(vec![
                        ">> ".bold().fg(Color::Green),
                        text.clone().into(),
                    ]));
                }
                ChatLine::Assistant(text) => {
                    lines.extend(super::markdown::render_markdown(text, term_width));
                }
                ChatLine::ToolCall { name, args } => {
                    lines.push(Line::from(vec![
                        "[tool] ".dim().fg(Color::Yellow),
                        format!("{name}({args})").dim(),
                    ]));
                }
                ChatLine::ToolResult(result) => {
                    lines.push(Line::from(vec![
                        "[result] ".dim().fg(Color::Cyan),
                        result.clone().dim(),
                    ]));
                }
                ChatLine::Reasoning(text) => {
                    lines.push(Line::from(vec![
                        "[think] ".dim().fg(Color::Magenta),
                        text.clone().italic().dim(),
                    ]));
                }
                ChatLine::Status(msg) => {
                    lines.push(Line::from(msg.clone().dim().italic()));
                }
                ChatLine::Error(err) => {
                    lines.push(Line::from(vec![
                        "[error] ".fg(Color::Red),
                        err.clone().fg(Color::Red),
                    ]));
                }
                ChatLine::System(msg) => {
                    for (i, line) in msg.lines().enumerate() {
                        let prefix = if i == 0 { "[system] " } else { "          " };
                        lines.push(Line::from(vec![
                            prefix.fg(Color::DarkGray),
                            line.to_owned().fg(Color::DarkGray),
                        ]));
                    }
                }
            }
        }
        Text::from(lines)
    }
}
