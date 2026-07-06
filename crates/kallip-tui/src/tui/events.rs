use ratatui::crossterm::event::{MouseEvent, MouseEventKind};

use kallip_common::protocol::SseEvent;
use kallip_common::tokens::format_tokens_m;

use super::{App, AppMode, ChatLine};

impl App {
    /// Handle an SSE event from the daemon.
    pub fn handle_sse_event(&mut self, event: SseEvent) {
        // A "boundary" marks a point where the daemon can interject a queued
        // prompt: a `ToolCall` (the assistant committed tool calls, ending this
        // streamed message) or a terminal event. The daemon's
        // `drain_interjections` runs at the top of the next round iteration
        // (after the current tool batch), so flushing here lands the prompt in
        // time. Transient `Failover`/`Retrying` are within-stream retries, not
        // message boundaries.
        let is_boundary = matches!(
            event,
            SseEvent::ToolCall { .. }
                | SseEvent::Finished { .. }
                | SseEvent::Cancelled
                | SseEvent::Interrupted
                | SseEvent::Error { .. }
                | SseEvent::MaxRoundsExceeded
                | SseEvent::FailoverChainExhausted { .. }
                | SseEvent::TokenBudgetExceeded { .. }
        );
        match event {
            SseEvent::Reasoning { content } => {
                self.chat_lines.push(ChatLine::Reasoning(content));
                self.auto_scroll = true;
            }
            SseEvent::AssistantContent { content } => {
                self.chat_lines.push(ChatLine::Assistant(content));
                self.auto_scroll = true;
            }
            SseEvent::AssistantContentDelta { delta } => {
                self.streaming_content = true;
                if let Some(ChatLine::Assistant(existing)) = self.chat_lines.last_mut() {
                    existing.push_str(&delta);
                } else {
                    self.chat_lines.push(ChatLine::Assistant(delta));
                }
                self.auto_scroll = true;
            }
            SseEvent::ReasoningDelta { delta } => {
                self.streaming_reasoning = true;
                if let Some(ChatLine::Reasoning(existing)) = self.chat_lines.last_mut() {
                    existing.push_str(&delta);
                } else {
                    self.chat_lines.push(ChatLine::Reasoning(delta));
                }
                self.auto_scroll = true;
            }
            SseEvent::ToolCall { name, args } => {
                self.chat_lines.push(ChatLine::ToolCall { name, args });
                self.auto_scroll = true;
            }
            SseEvent::ToolResult { result } => {
                self.chat_lines.push(ChatLine::ToolResult(result));
                self.auto_scroll = true;
            }
            SseEvent::Finished { content } => {
                if !self.streaming_content && !content.is_empty() {
                    self.chat_lines.push(ChatLine::Assistant(content));
                }
                self.streaming_content = false;
                self.streaming_reasoning = false;
                self.agent_busy = false;
                self.auto_scroll = true;
            }
            SseEvent::MaxRoundsExceeded => {
                self.chat_lines
                    .push(ChatLine::Error("max rounds exceeded".into()));
                self.agent_busy = false;
                self.auto_scroll = true;
            }
            SseEvent::Error { message } => {
                self.chat_lines.push(ChatLine::Error(message));
                self.agent_busy = false;
                self.auto_scroll = true;
            }
            SseEvent::Status { message } => {
                self.chat_lines.push(ChatLine::Status(message));
                self.auto_scroll = true;
            }
            SseEvent::Busy => {
                self.agent_busy = true;
                self.streaming_content = false;
                self.streaming_reasoning = false;
            }
            SseEvent::ApprovalUpdated { id, status } => {
                if matches!(self.mode, AppMode::Approvals) {
                    if let Some(state) = self.approvals.as_mut() {
                        state.stale = true;
                    }
                } else {
                    self.chat_lines
                        .push(ChatLine::Status(format!("[approval] {id}: {status}")));
                    self.auto_scroll = true;
                }
            }
            SseEvent::Retrying {
                attempt,
                max_attempts,
                error,
                delay_secs,
            } => {
                self.chat_lines.push(ChatLine::Retrying {
                    attempt,
                    max_attempts,
                    error,
                    delay_secs,
                });
                self.auto_scroll = true;
            }
            SseEvent::Failover { from, to, reason } => {
                self.chat_lines
                    .push(ChatLine::Failover { from, to, reason });
                self.auto_scroll = true;
            }
            SseEvent::FailoverChainExhausted { reason, detail } => {
                self.chat_lines.push(ChatLine::FailoverExhausted {
                    reason: reason.to_string(),
                    detail,
                });
                self.agent_busy = false;
                self.streaming_content = false;
                self.streaming_reasoning = false;
                self.auto_scroll = true;
            }
            SseEvent::Cancelled => {
                self.chat_lines
                    .push(ChatLine::System("Operation cancelled".into()));
                self.agent_busy = false;
                self.streaming_content = false;
                self.streaming_reasoning = false;
                self.auto_scroll = true;
            }
            SseEvent::Interrupted => {
                self.chat_lines
                    .push(ChatLine::System("Operation interrupted".into()));
                self.agent_busy = false;
                self.streaming_content = false;
                self.streaming_reasoning = false;
                self.auto_scroll = true;
            }
            SseEvent::TokenBudgetExceeded { consumed, budget } => {
                self.chat_lines.push(ChatLine::Error(format!(
                    "Token budget exceeded: {} / {}",
                    format_tokens_m(consumed),
                    format_tokens_m(budget)
                )));
                self.agent_busy = false;
                self.streaming_content = false;
                self.streaming_reasoning = false;
                self.auto_scroll = true;
            }
        }

        // After a boundary, hand any queued input to the main loop for sending.
        // `request_flush` no-ops when nothing is pending or the outbox is busy.
        if is_boundary {
            self.request_flush();
        }
    }

    /// Handle a mouse scroll event in the chat area.
    pub fn handle_mouse_event(&mut self, event: MouseEvent, _chat_area_height: u16) {
        match event.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_pos = self.scroll_pos.saturating_sub(3);
                self.auto_scroll = false;
            }
            MouseEventKind::ScrollDown => {
                self.scroll_pos = self.scroll_pos.saturating_add(3);
                // Re-enable auto_scroll if scrolled to bottom
                let max_pos = self.content_length.saturating_sub(self.visible_height);
                if self.scroll_pos >= max_pos {
                    self.auto_scroll = true;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_common::protocol::SseEvent;

    /// Assert a boundary event flushes pending input into the outbox.
    fn assert_boundary_flushes(event: SseEvent) {
        let mut app = App::new();
        app.pending.push("queued".into());
        app.handle_sse_event(event);
        assert_eq!(app.outbox.as_deref(), Some("queued"));
    }

    /// Assert a non-boundary event leaves pending unflushed.
    fn assert_non_boundary_keeps_pending(event: SseEvent) {
        let mut app = App::new();
        app.pending.push("queued".into());
        app.handle_sse_event(event);
        assert!(app.outbox.is_none(), "unexpected flush");
        assert_eq!(app.pending, vec!["queued".to_string()]);
    }

    #[test]
    fn tool_call_is_a_boundary() {
        assert_boundary_flushes(SseEvent::ToolCall {
            name: "cat".into(),
            args: "{}".into(),
        });
    }

    #[test]
    fn finished_is_a_boundary() {
        assert_boundary_flushes(SseEvent::Finished {
            content: "done".into(),
        });
    }

    #[test]
    fn interrupted_is_a_boundary() {
        assert_boundary_flushes(SseEvent::Interrupted);
    }

    #[test]
    fn assistant_delta_is_not_a_boundary() {
        assert_non_boundary_keeps_pending(SseEvent::AssistantContentDelta {
            delta: "chunk".into(),
        });
    }

    #[test]
    fn busy_is_not_a_boundary() {
        assert_non_boundary_keeps_pending(SseEvent::Busy);
    }
}
