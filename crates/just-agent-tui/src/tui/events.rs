use ratatui::crossterm::event::{MouseEvent, MouseEventKind};

use just_agent_client::DeferredInfo;
use just_agent_core::types::SseEvent;

use super::{App, ChatLine};

impl App {
    /// Show approval popup from SSE DeferredCreated event data.
    pub fn show_approval(&mut self, info: DeferredInfo) {
        self.approval.show(info);
    }

    /// Push an error into chat lines.
    pub fn push_error(&mut self, msg: String) {
        self.chat_lines.push(ChatLine::Error(msg));
        self.auto_scroll = true;
    }

    /// Handle an SSE event from the daemon.
    pub fn handle_sse_event(&mut self, event: SseEvent) {
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
            SseEvent::DeferredCreated {
                request_id,
                tool_name,
                summary,
                reason,
                dangerous,
            } => {
                self.show_approval(DeferredInfo {
                    request_id,
                    tool_name,
                    summary,
                    reason,
                    dangerous,
                });
            }
            SseEvent::DeferredApproved { request_id } => {
                self.chat_lines.push(ChatLine::Status(format!(
                    "deferred action {request_id} approved"
                )));
                self.auto_scroll = true;
            }
            SseEvent::DeferredDenied { request_id, reason } => {
                self.chat_lines.push(ChatLine::Error(format!(
                    "deferred action {request_id} denied: {reason}"
                )));
                self.auto_scroll = true;
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
            SseEvent::Cancelled => {
                self.chat_lines
                    .push(ChatLine::System("Operation cancelled".into()));
                self.agent_busy = false;
                self.streaming_content = false;
                self.streaming_reasoning = false;
                self.auto_scroll = true;
            }
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
