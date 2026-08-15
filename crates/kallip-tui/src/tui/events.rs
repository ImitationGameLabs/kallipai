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
                | SseEvent::Idle
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
                // A `kallip lesche send` CLI call announces itself with a stable
                // stdout marker ({"kallip.lesche.message":{"text":...}}); render
                // that as the actual assistant chat line. Any other tool result
                // renders verbatim.
                if let Some(message) = parse_message_marker(&result) {
                    self.chat_lines.push(ChatLine::Assistant(message));
                } else {
                    self.chat_lines.push(ChatLine::ToolResult(result));
                }
                self.auto_scroll = true;
            }
            SseEvent::Idle => {
                // The agent yielded control. Content-less: a message to the user
                // is a deliberate `kallip lesche send` call (rendered from its
                // ToolResult marker above), not the final assistant message.
                // Finalize any in-flight streaming text and mark the agent idle.
                self.finalize_streaming();
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
                // `Assistant`/`Reasoning`); these flags only gate `Idle`'s finalize, but clearing
                // them keeps the "is a turn streaming?" state truthful after a void.
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

/// The stable stdout marker the `kallip lesche send` CLI prints, recognized in
/// any `ToolResult` so the TUI renders the agent's deliberate message as an
/// assistant chat line. Keyed by the marker prefix (not the tool name) so the
/// call survives shell wrappers, aliases, and arg-shape variation. Returns the
/// message text on a match, `None` for any other tool result.
///
/// The CLI always emits exactly one JSON line of this shape as its stdout; the
/// envelope wrapper around it (the executor's `{"ok":true,"tool_name":...,
/// "result":<output>}`) is parsed best-effort — the marker is matched anywhere in
/// the result string so a wrapper that embeds the CLI's stdout verbatim still
/// recognizes it.
fn parse_message_marker(result: &str) -> Option<String> {
    // The marker object embeds the key "kallip.lesche.message"; find it anywhere
    // in the result, then parse the enclosing {...} object (robust to the
    // executor's wrapper and to shell wrappers around the CLI call).
    const MARKER: &str = r#""kallip.lesche.message""#;
    let idx = result.find(MARKER)?;
    let start = result[..idx].rfind('{')?;
    // Walk braces from `start` to the matching close, tolerating nested
    // objects and strings, then parse and extract `.["kallip.lesche.message"].text`.
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, ch) in result[start..].char_indices() {
        if in_str {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let obj: serde_json::Value =
                        serde_json::from_str(&result[start..start + i + 1]).ok()?;
                    return obj
                        .get("kallip.lesche.message")
                        .and_then(|m| m.get("text"))
                        .and_then(|t| t.as_str())
                        .map(String::from);
                }
            }
            _ => {}
        }
    }
    None
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

    /// The agent-message echo must never parse as a lesche marker, even
    /// when the sent text is pathologically the marker key itself: the
    /// needle stage hits (the key appears as a JSON value), but the brace
    /// walk yields the `kallip.message.sent` object, which has no
    /// `kallip.lesche.message` key — so no user chat line renders.
    #[test]
    fn message_sent_echo_is_not_a_lesche_marker() {
        let echo =
            kallip_common::message::message_sent_line("id-1", "kallip.lesche.message", 0, None);
        assert!(parse_message_marker(&echo).is_none());
    }

    #[test]
    fn tool_call_is_a_boundary() {
        assert_boundary_flushes(SseEvent::ToolCall {
            name: "cat".into(),
            args: "{}".into(),
        });
    }

    #[test]
    fn idle_is_a_boundary() {
        assert_boundary_flushes(SseEvent::Idle);
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
