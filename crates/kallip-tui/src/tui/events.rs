use kallip_common::protocol::SseEvent;
use kallip_common::tokens::format_tokens_m;

use super::{App, AppMode, ChatLine};

impl App {
    /// Handle an SSE event from the tagma.
    ///
    /// Returns `true` when the event is a streaming content/reasoning delta —
    /// the high-frequency case the main loop coalesces into a frame-rate-capped
    /// redraw. All other events (boundaries, tool events, errors) return
    /// `false` and redraw immediately. The state mutation has already happened
    /// by the time this returns; a `true` only defers the *draw* to the frame
    /// cap, so the final state is always correct.
    pub fn handle_sse_event(&mut self, event: SseEvent) -> bool {
        // A "boundary" marks a point where the tagma can interject a queued
        // prompt: a `ToolCall` (the assistant committed tool calls, ending this
        // streamed message) or a terminal event. The tagma's
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
        let mut is_delta = false;
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
                self.append_streaming_delta(true, &delta);
                self.auto_scroll = true;
                is_delta = true;
            }
            SseEvent::ReasoningDelta { delta } => {
                self.streaming_reasoning = true;
                self.append_streaming_delta(false, &delta);
                self.auto_scroll = true;
                is_delta = true;
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
                // Finalize before clearing the flags: while a flag is still set,
                // the trailing entry is the in-flight partial whose cache slot
                // holds the deferred (unhighlighted) render and must be rebuilt.
                self.finalize_streaming();
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
                self.finalize_streaming();
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
                self.finalize_streaming();
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
                self.finalize_streaming();
                self.chat_lines
                    .push(ChatLine::System("Operation cancelled".into()));
                self.agent_busy = false;
                self.streaming_content = false;
                self.streaming_reasoning = false;
                self.auto_scroll = true;
            }
            SseEvent::Interrupted => {
                self.finalize_streaming();
                self.chat_lines
                    .push(ChatLine::System("Operation interrupted".into()));
                self.agent_busy = false;
                self.streaming_content = false;
                self.streaming_reasoning = false;
                self.auto_scroll = true;
            }
            SseEvent::TokenBudgetExceeded { consumed, budget } => {
                self.finalize_streaming();
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
            SseEvent::StreamReset {
                error,
                attempt,
                max_attempts,
                delay_secs,
            } => {
                // The stream dropped mid-way and the runtime is retrying from scratch. Fold the
                // trailing partial assistant/reasoning entries this turn streamed (keep them in
                // history, collapsed, for traceability) so the retried stream renders fresh
                // below — do NOT overwrite. Walk the tail until a non-streaming entry.
                for idx in (0..self.chat_lines.len()).rev() {
                    match &self.chat_lines[idx] {
                        ChatLine::Assistant(_) | ChatLine::Reasoning(_) => {
                            if self.collapsed.insert(idx) {
                                // Force a re-render so the now-collapsed entry shows folded.
                                if let Some(slot) = self.render_cache.get_mut(idx) {
                                    *slot = None;
                                }
                            }
                        }
                        _ => break,
                    }
                }
                // Clear the streaming flags: the pushed `StreamDropped` line is what makes the
                // next delta start a fresh entry (`append_streaming_delta` no longer tail-matches
                // `Assistant`/`Reasoning`); these flags only gate `Finished`'s dedup gate, but
                // clearing them keeps the "is a turn streaming?" state truthful after a void.
                //
                // `finalize_streaming()` is intentionally NOT called here, unlike the other
                // flag-clearing arms: the tail-walk above already invalidated every slot it
                // touched, and `finalize_streaming`'s "is the last entry Assistant/Reasoning?"
                // guard would be wrong now that those partials are collapsed.
                self.streaming_content = false;
                self.streaming_reasoning = false;
                self.chat_lines.push(ChatLine::StreamDropped {
                    attempt,
                    max_attempts,
                    error,
                    delay_secs,
                });
                self.auto_scroll = true;
            }
        }

        // After a boundary, hand any queued input to the main loop for sending.
        // `request_flush` no-ops when nothing is pending or the outbox is busy.
        if is_boundary {
            self.request_flush();
        }

        is_delta
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

    #[test]
    fn delta_events_signal_coalescable_redraw() {
        // The frame-rate cap coalesces only streaming deltas; everything else
        // redraws immediately. `handle_sse_event` reports which via its return.
        let mut app = App::new();
        assert!(
            app.handle_sse_event(SseEvent::AssistantContentDelta { delta: "a".into() }),
            "content delta is coalescable"
        );
        assert!(
            app.handle_sse_event(SseEvent::ReasoningDelta { delta: "b".into() }),
            "reasoning delta is coalescable"
        );
        assert!(
            !app.handle_sse_event(SseEvent::Busy),
            "non-delta events redraw immediately"
        );
    }

    #[test]
    fn stream_reset_is_not_a_boundary() {
        assert_non_boundary_keeps_pending(SseEvent::StreamReset {
            error: "boom".into(),
            attempt: 1,
            max_attempts: 2,
            delay_secs: 0.1,
        });
    }

    #[test]
    fn stream_reset_folds_partial_and_starts_fresh() {
        let mut app = App::new();
        // Stream a partial assistant turn, then the stream drops mid-way.
        app.handle_sse_event(SseEvent::AssistantContentDelta {
            delta: "part1-".into(),
        });
        app.handle_sse_event(SseEvent::AssistantContentDelta {
            delta: "part2".into(),
        });
        assert_eq!(
            app.chat_lines.len(),
            1,
            "deltas coalesce into one Assistant entry"
        );
        app.handle_sse_event(SseEvent::StreamReset {
            error: "boom".into(),
            attempt: 1,
            max_attempts: 2,
            delay_secs: 0.1,
        });
        // The abandoned partial stays in history but is collapsed, and a report line is pushed.
        assert!(matches!(
            app.chat_lines.last(),
            Some(ChatLine::StreamDropped { .. })
        ));
        assert!(
            app.collapsed.contains(&0),
            "the abandoned partial is marked collapsed"
        );
        assert!(
            !app.streaming_content,
            "streaming flag cleared for a fresh entry"
        );
        // The retried stream's first delta starts a NEW entry — not appended to the voided tail.
        app.handle_sse_event(SseEvent::AssistantContentDelta {
            delta: "fresh".into(),
        });
        assert_eq!(
            app.chat_lines.len(),
            3,
            "[Assistant(partial), StreamDropped, Assistant(fresh)]"
        );
        assert!(matches!(
            app.chat_lines.last(),
            Some(ChatLine::Assistant(s)) if s == "fresh"
        ));
    }
}
