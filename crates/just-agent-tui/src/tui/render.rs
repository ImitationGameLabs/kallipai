use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use super::wrap::word_wrap_line_count;
use super::{App, AppMode, ApprovalPhase, ChatLine};

impl App {
    /// Render the TUI.
    pub fn render(&mut self, frame: &mut Frame) {
        match self.mode {
            AppMode::Chat => self.render_chat(frame),
            AppMode::Approvals => self.render_approvals(frame),
        }
    }

    fn render_chat(&mut self, frame: &mut Frame) {
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
                chat_area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
        }

        self.scroll_pos = pos;
        self.content_length = total;
        self.visible_height = visible_height;

        self.completion.render(frame, input_area);
        if self.quit_confirm {
            self.render_quit_popup(frame, input_area);
        }
        frame.render_widget(&self.textarea, input_area);
    }

    fn render_approvals(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let Some(state) = &self.approvals else {
            return;
        };

        let count = state.entries.len();
        let title = format!("Approvals ({count} committed)");

        let content_width = area.width.saturating_sub(2) as usize;
        let rows: Vec<Line> = state
            .entries
            .iter()
            .enumerate()
            .flat_map(|(i, entry)| {
                let id_short = &entry.id[..12.min(entry.id.len())];
                let age = format_age(entry.created_at);

                let header = if i == state.selected {
                    Line::from(vec![
                        Span::styled(
                            format!(" {id_short}  "),
                            Style::default().add_modifier(Modifier::REVERSED),
                        ),
                        Span::styled(
                            format!("{:<20} ", entry.content.tool_name),
                            Style::default().add_modifier(Modifier::REVERSED),
                        ),
                        Span::styled(
                            format!("{age} "),
                            Style::default().add_modifier(Modifier::REVERSED),
                        ),
                    ])
                } else {
                    Line::from(vec![
                        format!(" {id_short}  ").into(),
                        format!("{:<20} ", entry.content.tool_name).into(),
                        age.dim(),
                    ])
                };

                let args_str = format_json_compact(&entry.content.arguments, content_width);
                let arg_line = Line::from(format!("   args: {args_str}").dim());

                let mut lines = vec![header, arg_line];
                if let Some(ref reason) = entry.commit_reason {
                    lines.push(Line::from(format!("   reason: {reason}").dim()));
                }

                lines
            })
            .collect();

        let hint_height = 3u16;
        let list_height = area.height.saturating_sub(hint_height);
        let list_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: list_height,
        };
        let hint_area = Rect {
            x: area.x,
            y: area.y + list_height,
            width: area.width,
            height: hint_height,
        };

        let list = Paragraph::new(rows).block(Block::bordered().title(title));
        frame.render_widget(Clear, area);
        frame.render_widget(list, list_area);

        // Bottom hint bar
        let hint = match &state.phase {
            ApprovalPhase::Browsing => {
                if count == 0 {
                    if state.stale {
                        Line::from(vec![
                            "No pending approvals. ".dark_gray(),
                            "list updated".yellow(),
                            "  ".into(),
                            "r".bold(),
                            " refresh  ".into(),
                            "Esc".bold(),
                            " back".into(),
                        ])
                    } else {
                        "No pending approvals. Esc to go back.".dark_gray().into()
                    }
                } else if state.stale {
                    Line::from(vec![
                        "↑/↓".bold(),
                        " select  ".into(),
                        "Space".bold(),
                        " decide  ".into(),
                        "r".bold(),
                        " refresh  ".into(),
                        "list updated".yellow(),
                        "  ".into(),
                        "Esc".bold(),
                        " back".into(),
                    ])
                } else {
                    Line::from(vec![
                        "↑/↓".bold(),
                        " select  ".into(),
                        "Space".bold(),
                        " decide  ".into(),
                        "Esc".bold(),
                        " back".into(),
                    ])
                }
            }
            ApprovalPhase::Deciding => {
                let entry = &state.entries[state.selected];
                Line::from(vec![
                    "[".dark_gray(),
                    entry.id[..12.min(entry.id.len())].to_string().yellow(),
                    "] ".dark_gray(),
                    "1".bold(),
                    " approve  ".into(),
                    "2".bold(),
                    " deny  ".into(),
                    "Esc".bold(),
                    " cancel".into(),
                ])
            }
            ApprovalPhase::DenyInput { buffer } => {
                let entry = &state.entries[state.selected];
                Line::from(vec![
                    "[".dark_gray(),
                    entry.id[..12.min(entry.id.len())].to_string().yellow(),
                    "] ".dark_gray(),
                    "deny reason: ".into(),
                    buffer.clone().fg(Color::Yellow),
                    "_".fg(Color::Yellow),
                    "  ".into(),
                    "Enter".bold(),
                    " submit  ".into(),
                    "Esc".bold(),
                    " cancel".into(),
                ])
            }
        };
        frame.render_widget(
            Paragraph::new(hint).block(Block::bordered().style(ratatui::style::Style::default())),
            hint_area,
        );
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
                ChatLine::Retrying {
                    attempt,
                    max_attempts,
                    error,
                    delay_secs,
                } => {
                    lines.push(Line::from(vec![
                        "\u{27F3} ".dim(),
                        format!("retrying ({attempt}/{max_attempts}): ").dim(),
                        format!("{error} \u{2014} waiting {delay_secs:.1}s")
                            .dim()
                            .italic(),
                    ]));
                }
                ChatLine::Failover { from, to, reason } => {
                    lines.push(Line::from(vec![
                        "\u{21C4} ".dim().fg(Color::Yellow),
                        "[failover] ".dim().fg(Color::Yellow),
                        format!("{from} \u{2192} {to}: {reason}").dim(),
                    ]));
                }
                ChatLine::FailoverExhausted { reason, detail } => {
                    lines.push(Line::from(vec![
                        "[failover exhausted] ".fg(Color::Red),
                        format!("{reason}: {detail}").fg(Color::Red),
                    ]));
                }
            }
        }
        Text::from(lines)
    }

    fn render_quit_popup(&self, frame: &mut Frame, input_area: Rect) {
        let width = 37.min(input_area.width);
        let height = 7u16;
        let popup_area = Rect {
            x: input_area.x + (input_area.width.saturating_sub(width)) / 2,
            y: input_area.y.saturating_sub(height),
            width,
            height,
        };
        frame.render_widget(Clear, popup_area);

        let lines = vec![
            Line::from(""),
            Line::from("  [1] Keep agent running and quit"),
            Line::from("  [2] Delete agent and quit"),
            Line::from(""),
            Line::from("  Esc to cancel".dark_gray()),
        ];

        let popup = Paragraph::new(lines)
            .block(Block::bordered().title(" Quit ").yellow())
            .wrap(Wrap { trim: true });
        frame.render_widget(popup, popup_area);
    }
}

/// Format an OffsetDateTime as a human-readable relative age.
/// Format a timestamp as a short relative age string (e.g. "3s", "5m", "2h", "1d").
/// Used in the approvals list to show when each approval was created.
/// Returns "0s" for timestamps at or after the current time.
fn format_age(t: time::OffsetDateTime) -> String {
    let now = time::OffsetDateTime::now_utc();
    let delta = now - t;
    if delta.whole_seconds() < 60 {
        format!("{}s", delta.whole_seconds())
    } else if delta.whole_minutes() < 60 {
        format!("{}m", delta.whole_minutes())
    } else if delta.whole_hours() < 24 {
        format!("{}h", delta.whole_hours())
    } else {
        format!("{}d", delta.whole_days())
    }
}

/// Format a JSON value for display in the approvals list.
/// Objects and arrays use compact pretty-print; scalars use default formatting.
fn format_json_compact(v: &serde_json::Value, max_width: usize) -> String {
    let s = match v {
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
        }
        _ => v.to_string(),
    };
    if s.len() <= max_width {
        s
    } else {
        format!("{}...", &s[..max_width.saturating_sub(3)])
    }
}
