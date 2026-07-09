use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::command;
use kallip_client::DaemonClient;
use kallip_client::ListApprovalsParams;
use kallip_common::agentid::AgentId;
use kallip_common::command::{BudgetOp, SlashCommand};

use super::{App, AppMode, ApprovalPhase, ChatLine};

/// Lines scrolled per `Up`/`Down` key (and per mouse-wheel tick, which the terminal
/// delivers as `Up`/`Down` via alternate-scroll).
const SCROLL_STEP_LINES: usize = 3;
/// Lines scrolled per `PageUp`/`PageDown`.
const PAGE_SCROLL_STEP_LINES: usize = 10;

impl App {
    /// Handle a crossterm key event.
    pub async fn handle_key_event(
        &mut self,
        key: KeyEvent,
        client: &DaemonClient,
        agent_id: &AgentId,
    ) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        // Approvals view handles its own keys.
        if matches!(self.mode, AppMode::Approvals) {
            self.handle_approvals_key(key, client).await;
            return;
        }

        // Quit confirmation popup: intercept 1/2/Esc
        if self.quit_confirm {
            match key.code {
                KeyCode::Char('1') => {
                    self.kill_on_exit = false;
                    self.should_quit = true;
                }
                KeyCode::Char('2') => {
                    self.kill_on_exit = true;
                    self.should_quit = true;
                }
                KeyCode::Esc => {
                    self.quit_confirm = false;
                }
                _ => {}
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

        // Ctrl-O toggles folding of tool/reasoning content.
        if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.toggle_fold();
            return;
        }

        // Ctrl-P / Ctrl-N recall input history (previous / next). Up/Down scroll the
        // chat because the mouse wheel arrives as Up/Down via alternate-scroll; history
        // is bound to Ctrl-P/Ctrl-N so the wheel does not cycle it.
        if !self.completion.is_visible() && key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('p') => {
                    let current = self.textarea.lines().join("\n");
                    if let Some(entry) = self.history.up(&current) {
                        self.textarea.clear();
                        self.textarea.insert_str(entry);
                        self.refresh_completion();
                    }
                    return;
                }
                KeyCode::Char('n') => {
                    if let Some(result) = self.history.down() {
                        self.textarea.clear();
                        self.textarea.insert_str(result.as_str());
                        self.refresh_completion();
                    }
                    return;
                }
                _ => {}
            }
        }

        // Scroll keys. PageUp/PageDown scroll the chat, never the textarea. The
        // same goes for Ctrl+V / Alt+V, which ratatui-textarea's default input
        // map would otherwise consume as PageDown/PageUp viewport scroll: letting
        // them reach `textarea.input` would scroll the textarea's private viewport
        // and desync the caret-positioning mirror in `render_chat` (which assumes
        // the textarea viewport only ever moves via cursor follow).
        match key.code {
            KeyCode::PageUp => {
                self.scroll_up_by(PAGE_SCROLL_STEP_LINES);
                return;
            }
            KeyCode::PageDown => {
                self.scroll_down_by(PAGE_SCROLL_STEP_LINES);
                return;
            }
            KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.scroll_up_by(PAGE_SCROLL_STEP_LINES);
                return;
            }
            KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_down_by(PAGE_SCROLL_STEP_LINES);
                return;
            }
            _ => {}
        }

        // Chat scrolling via Up/Down (when the completion popup is not visible - the
        // popup handles its own Up/Down navigation below). The mouse wheel is delivered
        // as Up/Down by alternate-scroll, so this is what makes the wheel scroll chat.
        if !self.completion.is_visible() {
            match key.code {
                KeyCode::Up => {
                    self.scroll_up_by(SCROLL_STEP_LINES);
                    return;
                }
                KeyCode::Down => {
                    self.scroll_down_by(SCROLL_STEP_LINES);
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

            // Empty submit with queued input acts as a manual flush/retry trigger
            // (e.g. resending after a failure once the agent is idle).
            if text.is_empty() {
                if !self.pending.is_empty() {
                    self.request_flush();
                }
                return;
            }

            match command::parse(&text) {
                None => {
                    // Free-text prompt: queue it. While busy it merges with prior
                    // queued input and is sent at the next daemon interjection
                    // boundary; while idle it is flushed immediately as its own
                    // turn. Rendering happens in the main loop when it drains the
                    // outbox, so the user line lands after the boundary event
                    // rather than interleaved with the in-progress stream.
                    self.history.push(text.clone());
                    self.textarea.clear();
                    self.completion.hide();
                    self.auto_scroll = true;
                    self.pending.push(text);
                    if !self.agent_busy {
                        self.request_flush();
                    }
                }
                Some(Ok(cmd)) => {
                    // Slash commands require an idle agent. While busy, reject
                    // without touching chat_lines: pushing a line mid-stream would
                    // split the in-progress assistant line (deltas append to
                    // `last_mut`). Leave the text so the user can retry when idle.
                    if self.agent_busy {
                        return;
                    }
                    self.textarea.clear();
                    self.completion.hide();
                    self.dispatch_command(cmd, client, agent_id).await;
                }
                Some(Err(msg)) => {
                    self.textarea.clear();
                    self.completion.hide();
                    self.chat_lines.push(ChatLine::Error(msg));
                    self.auto_scroll = true;
                }
            }
            return;
        }

        // Forward all other keys to textarea, then update completion
        self.textarea.input(key);
        self.refresh_completion();
    }

    /// Re-derive completion candidates from the current textarea content.
    /// Shared by the per-key path above and [`App::apply_bracketed_paste`]
    /// (and, as a side effect, fixes a latent staleness in the Ctrl-P/Ctrl-N
    /// history-recall path, which previously skipped this).
    fn refresh_completion(&mut self) {
        let text = self.textarea.lines().join("\n");
        self.completion.update(&text);
    }

    /// Insert a bracketed-paste payload as a single batched edit.
    ///
    /// Named to avoid colliding with [`ratatui_textarea::TextArea::paste`], the
    /// kill-ring yank. Crossterm strips the `ESC[200~`/`ESC[201~` delimiters
    /// before delivering `Event::Paste`, so the text is inserted verbatim. One
    /// insertion + one completion refresh, regardless of payload size — the
    /// non-bracketed fallback (one char per `KeyEvent`) still applies on
    /// terminals without the mode.
    pub(crate) async fn apply_bracketed_paste(&mut self, text: String) {
        self.textarea.insert_str(text);
        self.refresh_completion();
    }

    /// Handle key events in the approvals view.
    async fn handle_approvals_key(&mut self, key: KeyEvent, client: &DaemonClient) {
        let Some(state) = self.approvals.as_mut() else {
            self.mode = AppMode::Chat;
            return;
        };

        match &mut state.phase {
            ApprovalPhase::Browsing => match key.code {
                KeyCode::Up => {
                    state.selected = state.selected.saturating_sub(1);
                }
                KeyCode::Down => {
                    if !state.entries.is_empty() {
                        state.selected = (state.selected + 1).min(state.entries.len() - 1);
                    }
                }
                KeyCode::Char(' ') => {
                    if !state.entries.is_empty() {
                        state.phase = ApprovalPhase::Deciding;
                    }
                }
                KeyCode::Char('r') => {
                    self.refresh_approvals(client).await;
                }
                KeyCode::Esc => {
                    self.mode = AppMode::Chat;
                    self.approvals = None;
                }
                _ => {}
            },
            ApprovalPhase::Deciding => match key.code {
                KeyCode::Char('1') => {
                    let id = state.entries[state.selected].id.clone();
                    match client.respond_approval(&id, "approve", None).await {
                        Ok(()) => self.refresh_approvals(client).await,
                        Err(e) => {
                            state.phase = ApprovalPhase::Browsing;
                            self.chat_lines.push(ChatLine::Error(e.to_string()));
                        }
                    }
                }
                KeyCode::Char('2') => {
                    state.phase = ApprovalPhase::DenyInput {
                        buffer: String::new(),
                    };
                }
                KeyCode::Esc => {
                    state.phase = ApprovalPhase::Browsing;
                }
                _ => {}
            },
            ApprovalPhase::DenyInput { buffer } => match key.code {
                KeyCode::Enter => {
                    if !buffer.is_empty() {
                        let reason = std::mem::take(buffer);
                        let id = state.entries[state.selected].id.clone();
                        match client.respond_approval(&id, "deny", Some(&reason)).await {
                            Ok(()) => self.refresh_approvals(client).await,
                            Err(e) => {
                                state.phase = ApprovalPhase::Browsing;
                                self.chat_lines.push(ChatLine::Error(e.to_string()));
                            }
                        }
                    }
                }
                KeyCode::Esc => {
                    state.phase = ApprovalPhase::Deciding;
                }
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Char(c) => {
                    buffer.push(c);
                }
                _ => {}
            },
        }
    }

    /// Re-fetch pending approvals from the daemon.
    async fn refresh_approvals(&mut self, client: &DaemonClient) {
        match client
            .list_approvals(&ListApprovalsParams {
                status: Some("committed".into()),
                limit: Some(20),
                ..Default::default()
            })
            .await
        {
            Ok(resp) => {
                if let Some(state) = self.approvals.as_mut() {
                    state.entries = resp.items;
                    state.selected = state.selected.min(state.entries.len().saturating_sub(1));
                    state.phase = ApprovalPhase::Browsing;
                    state.stale = false;
                }
            }
            Err(e) => {
                self.chat_lines.push(ChatLine::Error(e.to_string()));
            }
        }
    }

    /// Dispatch a parsed slash command.
    ///
    /// Two dispatch categories:
    /// - **TUI-local** (help/quit/clear): no daemon call, handled entirely here
    /// - **Daemon query** (status/approvals): request-response, awaits daemon endpoint directly
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
                self.clear_chat();
                // Drop any queued input and pending state along with the view;
                // otherwise a queued message would surface into the cleared chat.
                self.pending.clear();
                self.outbox = None;
                self.pending_send_failed = false;
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
            SlashCommand::Approvals => {
                match client
                    .list_approvals(&ListApprovalsParams {
                        status: Some("committed".into()),
                        limit: Some(20),
                        ..Default::default()
                    })
                    .await
                {
                    Ok(resp) => {
                        self.approvals = Some(super::ApprovalsState::new(resp.items));
                        self.mode = AppMode::Approvals;
                    }
                    Err(e) => {
                        self.chat_lines.push(ChatLine::Error(e.to_string()));
                        self.auto_scroll = true;
                    }
                }
            }
            SlashCommand::Budget { op } => match op {
                None => match client.get_token_budget().await {
                    Ok(resp) => {
                        self.chat_lines
                            .push(ChatLine::Status(resp.format_display()));
                        self.auto_scroll = true;
                    }
                    Err(e) => {
                        self.chat_lines.push(ChatLine::Error(e.to_string()));
                        self.auto_scroll = true;
                    }
                },
                Some(BudgetOp::Adjust(delta)) => match client.adjust_token_budget(delta).await {
                    Ok(resp) => {
                        let direction = if delta > 0 { "increased" } else { "decreased" };
                        self.chat_lines.push(ChatLine::Status(format!(
                            "Budget {direction}. {}",
                            resp.format_display()
                        )));
                        self.auto_scroll = true;
                    }
                    Err(e) => {
                        self.chat_lines
                            .push(ChatLine::Error(format!("Failed to adjust budget: {e}")));
                        self.auto_scroll = true;
                    }
                },
                Some(BudgetOp::Set(value)) => match client.set_token_budget(value).await {
                    Ok(resp) => {
                        self.chat_lines.push(ChatLine::Status(format!(
                            "Budget set. {}",
                            resp.format_display()
                        )));
                        self.auto_scroll = true;
                    }
                    Err(e) => {
                        self.chat_lines
                            .push(ChatLine::Error(format!("Failed to set budget: {e}")));
                        self.auto_scroll = true;
                    }
                },
            },
        }
    }
}
