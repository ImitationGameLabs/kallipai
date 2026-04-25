use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;

use super::super::Action;
use super::{App, ChatLine};
use just_agent_core::command::{self, SlashCommand};

impl App {
    /// Handle a crossterm key event.
    pub fn handle_key_event(&mut self, key: KeyEvent, action_tx: &mpsc::Sender<Action>) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        // Approval popup: intercept 1/2 keys
        if self.approval.is_pending() {
            if let KeyCode::Char(ch) = key.code {
                if let Some(decision_str) = self.approval.handle_key(ch)
                    && let Some(request_id) = self.approval.last_request_id()
                {
                    action_tx
                        .try_send(Action::RespondApproval {
                            request_id: request_id.to_owned(),
                            decision: decision_str,
                        })
                        .ok();
                }
            } else if key.code == KeyCode::Esc
                && let Some(decision_str) = self.approval.cancel()
                && let Some(request_id) = self.approval.last_request_id()
            {
                action_tx
                    .try_send(Action::RespondApproval {
                        request_id: request_id.to_owned(),
                        decision: decision_str,
                    })
                    .ok();
            }
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.completion.is_visible() {
                self.completion.hide();
            }
            return;
        }

        // Scroll keys
        match key.code {
            KeyCode::PageUp => {
                self.scroll_pos = self.scroll_pos.saturating_sub(10);
                self.auto_scroll = false;
                return;
            }
            KeyCode::PageDown => {
                self.scroll_pos = self.scroll_pos.saturating_add(10);
                self.auto_scroll = false;
                return;
            }
            _ => {}
        }

        // History navigation (when completion popup is not visible)
        if !self.completion.is_visible() {
            match key.code {
                KeyCode::Up => {
                    let current = self.textarea.lines().join("\n");
                    if let Some(entry) = self.history.up(&current) {
                        self.textarea.clear();
                        self.textarea.insert_str(entry);
                    }
                    return;
                }
                KeyCode::Down => {
                    if let Some(result) = self.history.down() {
                        self.textarea.clear();
                        match result {
                            super::history::Either::Entry(s) => {
                                self.textarea.insert_str(s);
                            }
                            super::history::Either::Draft(s) => {
                                self.textarea.insert_str(s);
                            }
                        }
                    }
                    return;
                }
                _ => {}
            }
        }

        // Completion popup navigation
        if self.completion.is_visible() {
            match key.code {
                KeyCode::Up => {
                    self.completion.move_up();
                    return;
                }
                KeyCode::Down => {
                    self.completion.move_down();
                    return;
                }
                KeyCode::Tab => {
                    if let Some(cmd) = self.completion.selected_command() {
                        self.textarea.clear();
                        self.textarea.insert_str(cmd.name);
                        self.textarea.insert_char(' ');
                        self.completion.hide();
                        return;
                    }
                }
                KeyCode::Esc => {
                    self.completion.hide();
                    return;
                }
                _ => {}
            }
        }

        // Enter submits input (unless Shift is held)
        if key.code == KeyCode::Enter
            && !key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL)
        {
            // If completion popup is visible, resolve to selected candidate first
            if self.completion.is_visible() {
                if let Some(cmd) = self.completion.selected_command() {
                    self.textarea.clear();
                    self.textarea.insert_str(cmd.name);
                }
                self.completion.hide();
            }

            let text = self.textarea.lines().join("\n");
            if !text.is_empty() && !self.agent_busy {
                self.auto_scroll = true;
                self.history.push(text.clone());
                self.textarea.clear();
                self.completion.hide();

                match command::parse(&text) {
                    None => {
                        self.chat_lines.push(ChatLine::User(text.clone()));
                        action_tx.try_send(Action::SendPrompt(text)).ok();
                    }
                    Some(Ok(cmd)) => {
                        self.dispatch_command(cmd, action_tx);
                    }
                    Some(Err(msg)) => {
                        self.chat_lines.push(ChatLine::Error(msg));
                    }
                }
            }
            return;
        }

        // Forward all other keys to textarea, then update completion
        self.textarea.input(key);
        let text = self.textarea.lines().join("\n");
        self.completion.update(&text);
    }

    /// Dispatch a parsed slash command.
    fn dispatch_command(&mut self, cmd: SlashCommand, action_tx: &mpsc::Sender<Action>) {
        match cmd {
            SlashCommand::Help => {
                self.chat_lines.push(ChatLine::System(command::help_text()));
                self.auto_scroll = true;
            }
            SlashCommand::Quit => {
                self.should_quit = true;
            }
            SlashCommand::Clear => {
                self.chat_lines.clear();
            }
            SlashCommand::Status => {
                action_tx
                    .try_send(Action::SendPrompt("/status".into()))
                    .ok();
                self.chat_lines
                    .push(ChatLine::System("requesting status...".into()));
                self.auto_scroll = true;
            }
            SlashCommand::Compact => {
                action_tx
                    .try_send(Action::SendPrompt("/compact".into()))
                    .ok();
                self.chat_lines
                    .push(ChatLine::System("running compaction...".into()));
                self.auto_scroll = true;
            }
            SlashCommand::Skill { name } => {
                action_tx
                    .try_send(Action::SendPrompt(format!("/skill {name}")))
                    .ok();
                self.chat_lines
                    .push(ChatLine::System(format!("loading skill: {name}...")));
                self.auto_scroll = true;
            }
        }
    }
}
