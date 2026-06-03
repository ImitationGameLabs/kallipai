//! Completion popup state and rendering for slash commands.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::command::{self, CommandInfo};

/// Tracks completion popup state: filtered candidates and selection.
pub struct CompletionState {
    visible: bool,
    candidates: Vec<&'static CommandInfo>,
    selected: usize,
}

impl CompletionState {
    pub fn new() -> Self {
        Self {
            visible: false,
            candidates: Vec::new(),
            selected: 0,
        }
    }

    /// Recompute candidates from the current textarea content.
    pub fn update(&mut self, text: &str) {
        let line = text.lines().next().unwrap_or("");
        if line.starts_with('/') && !line.contains(' ') {
            let matches = command::matching(line);
            if matches.is_empty() || (matches.len() == 1 && matches[0].name == line) {
                self.visible = false;
            } else {
                self.visible = true;
                self.candidates = matches;
                self.selected = 0;
            }
        } else {
            self.visible = false;
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected < self.candidates.len().saturating_sub(1) {
            self.selected += 1;
        }
    }

    /// Return the currently selected command, if any.
    pub fn selected_command(&self) -> Option<&'static CommandInfo> {
        if self.visible && !self.candidates.is_empty() {
            Some(self.candidates[self.selected])
        } else {
            None
        }
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// Render the popup as a floating overlay above the given area.
    pub fn render(&self, frame: &mut Frame, input_area: Rect) {
        if !self.visible || self.candidates.is_empty() {
            return;
        }

        let max_visible = self.candidates.len().min(6);
        let popup_height = max_visible as u16 + 2; // +2 for border
        let popup_width = 40u16.min(input_area.width);

        let popup_y = input_area.y.saturating_sub(popup_height);
        let popup_area = Rect {
            x: input_area.x + 1,
            y: popup_y,
            width: popup_width,
            height: popup_height,
        };

        // Clear the area behind the popup
        frame.render_widget(Clear, popup_area);

        let items: Vec<Line> = self
            .candidates
            .iter()
            .take(max_visible)
            .enumerate()
            .map(|(i, cmd)| {
                let style = if i == self.selected {
                    Style::default().fg(Color::Black).bg(Color::White)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Line::from(Span::styled(
                    format!(" {:<12} {}", cmd.name, cmd.description),
                    style,
                ))
            })
            .collect();

        let popup = Paragraph::new(items)
            .block(Block::bordered().border_style(Style::default().fg(Color::DarkGray)));
        frame.render_widget(popup, popup_area);
    }
}
