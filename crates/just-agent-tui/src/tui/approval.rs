//! Approval popup widget for TUI mode.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use just_agent_client::DeferredInfo;

/// Tracks pending approval state and renders the popup.
pub struct ApprovalState {
    info: Option<DeferredInfo>,
    /// Kept after resolve so the caller can retrieve the request_id.
    last_request_id: Option<String>,
}

impl ApprovalState {
    pub fn new() -> Self {
        Self {
            info: None,
            last_request_id: None,
        }
    }

    /// Show the approval popup for the given deferred action.
    pub fn show(&mut self, info: DeferredInfo) {
        self.last_request_id = Some(info.request_id.clone());
        self.info = Some(info);
    }

    pub fn is_pending(&self) -> bool {
        self.info.is_some()
    }

    /// Return the request_id of the most recently shown deferred action.
    pub fn last_request_id(&self) -> Option<&str> {
        self.last_request_id.as_deref()
    }

    /// Try to handle a key press. Returns `Some(decision_string)` if resolved.
    pub fn handle_key(&mut self, ch: char) -> Option<String> {
        let decision_str = match ch {
            '1' => "approve",
            '2' => "deny",
            _ => return None,
        };
        self.resolve(decision_str)
    }

    /// Resolve with Esc → deny. Returns the decision string.
    pub fn cancel(&mut self) -> Option<String> {
        self.resolve("deny")
    }

    fn resolve(&mut self, decision: &str) -> Option<String> {
        if self.info.take().is_some() {
            return Some(decision.to_owned());
        }
        None
    }

    /// Render the approval popup as a floating overlay above the input area.
    pub fn render(&self, frame: &mut Frame, input_area: Rect) {
        let Some(info) = &self.info else { return };

        let width = (input_area.width).min(60);
        let height = 8u16; // border(2) + tool + reason + summary + blank + options
        let popup_area = Rect {
            x: input_area.x + 1,
            y: input_area.y.saturating_sub(height),
            width,
            height,
        };

        frame.render_widget(Clear, popup_area);

        let summary_preview = truncate_str(&info.summary, (width - 4) as usize - 10);

        let (border_color, title, reason_style, options) = if info.dangerous {
            (
                Color::Red,
                " DANGER ",
                Color::Red,
                "  [1] Approve   [2] Deny",
            )
        } else {
            (
                Color::Yellow,
                " Approval ",
                Color::White,
                "  [1] Approve   [2] Deny",
            )
        };

        let lines = vec![
            Line::from(vec![
                Span::styled(" tool: ", Style::default().fg(border_color)),
                Span::styled(&info.tool_name, Style::default().fg(reason_style)),
            ]),
            Line::from(vec![
                Span::styled(" reason: ", Style::default().fg(border_color)),
                Span::styled(&info.reason, Style::default().fg(reason_style)),
            ]),
            Line::from(vec![
                Span::styled(" cmd: ", Style::default().fg(Color::DarkGray)),
                Span::styled(summary_preview, Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                options,
                Style::default()
                    .fg(if info.dangerous {
                        Color::Red
                    } else {
                        Color::Cyan
                    })
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )),
        ];

        let popup = Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(title)
                    .border_style(Style::default().fg(border_color)),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(popup, popup_area);
    }
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_owned()
    } else {
        let end = s
            .char_indices()
            .take(max_len.saturating_sub(1))
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}…", &s[..end])
    }
}
