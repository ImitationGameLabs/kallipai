use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;

use just_agent_client::DaemonClient;
use just_agent_core::command::{self, SlashCommand};
use just_agent_core::types::AgentId;

use super::super::Action;
use super::{App, ChatLine};

impl App {
    /// Handle a crossterm key event.
    pub async fn handle_key_event(
        &mut self,
        key: KeyEvent,
        action_tx: &mpsc::Sender<Action>,
        client: &DaemonClient,
        agent_id: &AgentId,
    ) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        // Quit confirmation popup: intercept 1/2/Esc
        if self.quit_confirm {
            match key.code {
                KeyCode::Char('1') => {
                    self.kill_on_exit = true;
                    self.should_quit = true;
                }
                KeyCode::Char('2') => {
                    self.kill_on_exit = false;
                    self.should_quit = true;
                }
                KeyCode::Esc => {
                    self.quit_confirm = false;
                }
                _ => {}
            }
            return;
        }

        // Approval popup: intercept 1/2/Esc keys
        if self.approval.is_pending() {
            let decision = if let KeyCode::Char(ch) = key.code {
                self.approval.handle_key(ch)
            } else if key.code == KeyCode::Esc {
                self.approval.cancel()
            } else {
                None
            };
            if let Some(decision_str) = decision
                && let Some(request_id) = self.approval.last_request_id()
                && let Err(e) = client
                    .respond_approval(agent_id, request_id, &decision_str, None)
                    .await
            {
                self.chat_lines.push(ChatLine::Error(e.to_string()));
                self.auto_scroll = true;
            }
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.completion.is_visible() {
                self.completion.hide();
            } else if self.agent_busy {
                if let Err(e) = client.interrupt_agent(agent_id).await {
                    self.chat_lines
                        .push(ChatLine::Error(format!("interrupt failed: {e}")));
                } else {
                    self.chat_lines
                        .push(ChatLine::System("Interrupting...".into()));
                }
                self.auto_scroll = true;
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
                        self.dispatch_command(cmd, client, agent_id).await;
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
    ///
    /// Two dispatch categories:
    /// - **TUI-local** (help/quit/clear): no daemon call, handled entirely here
    /// - **Daemon query** (status): request-response, awaits daemon endpoint directly
    async fn dispatch_command(
        &mut self,
        cmd: SlashCommand,
        client: &DaemonClient,
        agent_id: &AgentId,
    ) {
        match cmd {
            // TUI-local
            SlashCommand::Help => {
                self.chat_lines.push(ChatLine::System(command::help_text()));
                self.auto_scroll = true;
            }
            SlashCommand::Quit => {
                self.quit_confirm = true;
            }
            SlashCommand::Clear => {
                self.chat_lines.clear();
            }
            // Daemon query
            SlashCommand::Status => match client.agent_status(agent_id).await {
                Ok(status) => {
                    let mut msg = status.context.format_summary();
                    if !status.recent_retries.is_empty() {
                        msg.push_str(&format!(
                            "\n  retries: {} (last: {})",
                            status.recent_retries.len(),
                            status
                                .recent_retries
                                .first()
                                .map(|r| r.error.as_str())
                                .unwrap_or("n/a")
                        ));
                    }
                    self.chat_lines.push(ChatLine::Status(msg));
                    self.auto_scroll = true;
                }
                Err(e) => {
                    self.chat_lines.push(ChatLine::Error(e.to_string()));
                    self.auto_scroll = true;
                }
            },
        }
    }
}
