use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use super::wrap::word_wrap_line_count;
use super::{App, AppMode, ApprovalPhase, CachedEntry, ChatLine};

/// Maximum number of body lines rendered for a single tool call's arguments.
const MAX_TOOL_ARG_LINES: usize = 12;
/// Maximum number of body lines rendered for a single tool result. Results carry real
/// command output, so this is more generous than the args cap.
const MAX_TOOL_RESULT_LINES: usize = 50;
/// Per-line character cap, defending against binary / no-newline tool output that
/// would otherwise become one giant cached `Line`.
const MAX_TOOL_LINE_CHARS: usize = 4096;
/// Maximum column width used to align tool-call argument keys.
const MAX_TOOL_KEY_WIDTH: usize = 24;
/// Tool-result boolean flags that are noise when `false` but worth flagging in red
/// when `true`.
const TOOL_ALERT_KEYS: &[&str] = &["timed_out", "truncated"];
/// Header-bar background for a tool call. Blue reads as an action/information tint
/// and stays clear of yellow (warning) and the other reserved accents (cyan result,
/// magenta reasoning, red error, green user).
const TOOL_HEADER_BG: Color = Color::Blue;
/// Header-bar background for a tool result.
const RESULT_HEADER_BG: Color = Color::Cyan;

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

        let (text, total, header_marks) = self.build_chat_text(chat_area.width);

        let visible_height = chat_area.height.saturating_sub(2) as usize;

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

        // Paint tool call/result header bars. The Paragraph has already drawn the
        // header labels; this merges a background color across each header's row
        // (inside the border) so it reads as a solid bar. `set_style` only writes
        // the bg, leaving the label's symbol and fg intact. Only visible rows are
        // painted.
        let inner = chat_area.inner(Margin {
            vertical: 1,
            horizontal: 1,
        });
        for &(row, bg) in &header_marks {
            let dy = row as i64 - pos as i64;
            if (0..inner.height as i64).contains(&dy) {
                let bar = Rect {
                    x: inner.x,
                    y: inner.y + dy as u16,
                    width: inner.width,
                    height: 1,
                };
                frame.buffer_mut().set_style(bar, Style::default().bg(bg));
            }
        }

        // Scrollbar, only when content overflows viewport.
        let scroll_range = total.saturating_sub(visible_height);
        if scroll_range > 0 {
            let scrollbar_area = chat_area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            });
            // The cache stabilizes `total`/`pos` between real content changes, so
            // the thumb no longer jitters frame-to-frame — that alone removed the
            // scrollbar residue. (A `Clear` here would be wrong: this area spans
            // the full chat width, so it would wipe the paragraph text and both
            // side borders. The `Scrollbar` widget repaints the rightmost column
            // — track and thumb — every frame, which is all the clearing needed.)
            let mut scrollbar_state = ScrollbarState::new(scroll_range + 1)
                .position(pos)
                .viewport_content_length(visible_height);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                scrollbar_area,
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
        // Clear the textarea rect so stale per-line styling doesn't linger on
        // cells that this frame's text spans don't cover. Defense-in-depth now
        // that the cursor-line underline is disabled (see `App::new`).
        frame.render_widget(Clear, input_area);
        self.apply_input_title();
        frame.render_widget(&self.textarea, input_area);
    }

    /// Refresh the textarea border title to reflect queued input / send state.
    ///
    /// Called every chat frame; the block itself is built by [`App::input_block`]
    /// so only the title changes here.
    fn apply_input_title(&mut self) {
        let title = match (self.pending.len(), self.pending_send_failed) {
            (0, false) => ">> ".to_owned(),
            (0, true) => ">> send failed - Enter to retry ".to_owned(),
            (n, false) => format!(">> queued: {n} "),
            (n, true) => format!(">> queued: {n} (send failed - Enter to retry) "),
        };
        self.textarea.set_block(Self::input_block(title));
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

    /// Build the styled chat transcript, memoizing each entry's render output.
    ///
    /// Returns the assembled `Text`, the total wrapped row count, and the
    /// `(row, bg)` marks for each tool call/result header bar (consumed by the
    /// post-pass in [`render_chat`](Self::render_chat)). On a cache hit (an entry
    /// previously rendered at this width) the markdown/highlight work is skipped
    /// entirely; only the streaming tail and width-changed entries re-render.
    /// `total` comes from cached per-entry row counts, so the old
    /// whole-transcript `word_wrap_line_count` pass is gone.
    fn build_chat_text(&mut self, area_width: u16) -> (Text<'static>, usize, Vec<(usize, Color)>) {
        let content_width = area_width.saturating_sub(2);
        self.render_cache.resize(self.chat_lines.len(), None);

        let mut out: Vec<Line<'static>> = Vec::new();
        let mut total = 0usize;
        let mut header_marks: Vec<(usize, Color)> = Vec::new();
        for (i, entry) in self.chat_lines.iter().enumerate() {
            // The header bar (if any) is the entry's first visual row, which sits
            // at the current cumulative height — record it before adding the entry.
            let header_bg = match entry {
                ChatLine::ToolCall { .. } => Some(TOOL_HEADER_BG),
                ChatLine::ToolResult(_) => Some(RESULT_HEADER_BG),
                _ => None,
            };
            if let Some(bg) = header_bg {
                header_marks.push((total, bg));
            }

            if let Some(cached) = self.render_cache[i].as_ref()
                && cached.width == content_width
            {
                total += cached.wrapped_height;
                out.extend(cached.lines.iter().cloned());
            } else {
                let lines = render_one_entry(entry, area_width);
                let wrapped_height =
                    word_wrap_line_count(&Text::from(lines.clone()), content_width as usize);
                total += wrapped_height;
                let cached = CachedEntry {
                    width: content_width,
                    lines,
                    wrapped_height,
                };
                out.extend(cached.lines.iter().cloned());
                self.render_cache[i] = Some(cached);
            }
        }

        (Text::from(out), total, header_marks)
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
            Line::from("  [2] Remove agent and quit"),
            Line::from(""),
            Line::from("  Esc to cancel".dark_gray()),
        ];

        let popup = Paragraph::new(lines)
            .block(Block::bordered().title(" Quit ").yellow())
            .wrap(Wrap { trim: true });
        frame.render_widget(popup, popup_area);
    }
}

/// Render a single transcript entry into styled, unwrapped `Line`s.
///
/// Called only on a render-cache miss ([`App::build_chat_text`]); the result is
/// memoized per width. Word-wrapping of over-long lines is left to ratatui at
/// draw time. `area_width` is the border-inclusive width (markdown/table layout
/// subtracts its own border).
fn render_one_entry(entry: &ChatLine, area_width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    match entry {
        ChatLine::User(text) => {
            for (i, line) in text.lines().enumerate() {
                let prefix = if i == 0 { ">> " } else { "   " };
                lines.push(Line::from(vec![
                    prefix.bold().fg(Color::Green),
                    line.to_owned().into(),
                ]));
            }
        }
        ChatLine::Assistant(text) => {
            lines.extend(super::markdown::render_markdown(text, area_width));
        }
        ChatLine::ToolCall { name, args } => {
            // Header bar background is painted by the post-pass in `render_chat`
            // (the row mark is returned from `build_chat_text`); the label itself
            // just needs a readable fg on that bg. Cap the label to one row so the
            // background bar never splits across wrapped lines.
            let label = cap_chars(
                &format!("\u{258C}tool \u{00B7} {name}"),
                area_width.saturating_sub(2) as usize,
            );
            lines.push(Line::styled(label, Style::default().fg(Color::Black)));
            lines.extend(format_tool_args(args, MAX_TOOL_ARG_LINES));
        }
        ChatLine::ToolResult(result) => {
            // Header label is derived from the envelope (tool_name + status) and
            // capped to one row so the background bar never splits across lines.
            let label = tool_result_header_label(result);
            let header = cap_chars(&label, area_width.saturating_sub(2) as usize);
            lines.push(Line::styled(header, Style::default().fg(Color::Black)));
            if result.trim().is_empty() {
                lines.push(Line::from(Span::raw("  (empty)").dim()));
            } else {
                lines.extend(format_tool_result(result, MAX_TOOL_RESULT_LINES));
            }
        }
        ChatLine::Reasoning(text) => {
            lines.extend(prefixed_lines(
                "[think] ",
                text,
                Style::default().dim().italic(),
            ));
        }
        ChatLine::Status(msg) => {
            lines.extend(styled_lines(msg, Style::default().dim().italic()));
        }
        ChatLine::Error(err) => {
            lines.extend(prefixed_lines(
                "[error] ",
                err,
                Style::default().fg(Color::Red),
            ));
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
    lines
}

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

/// Pretty-print a raw JSON string into chat-view lines.
///
/// Valid JSON is re-serialized with 2-space indentation so structure is visible;
/// invalid input (or non-JSON tool output) falls back to the raw text split on
/// newlines. Each emitted line is capped at [`MAX_TOOL_LINE_CHARS`] (defense
/// against binary / no-newline output), and the total is bounded to `max_lines`
/// via [`bound_with_hint`].
fn format_json_pretty_lines(raw: &str, max_lines: usize) -> Vec<String> {
    let pretty = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok());
    let source_lines = match &pretty {
        Some(s) => s.lines(),
        None => raw.lines(),
    };
    let capped: Vec<String> = source_lines
        .map(|l| truncate_chars(l, MAX_TOOL_LINE_CHARS))
        .collect();
    bound_with_hint(capped, max_lines, more_lines_hint)
}

/// The "... (N more lines)" trailer used when bounded output is truncated. Shared so
/// the wording stays in one place across callers.
fn more_lines_hint(remaining: usize) -> String {
    format!("... ({remaining} more lines)")
}

/// Bound a list of lines to `max_lines`, replacing the tail with a single hint (built
/// by `mk_hint`) when truncated. Lines that fit are returned unchanged. Generic over
/// the element type so it serves both `Vec<String>` and `Vec<Line>` callers.
fn bound_with_hint<E, F>(mut lines: Vec<E>, max_lines: usize, mk_hint: F) -> Vec<E>
where
    F: Fn(usize) -> E,
{
    if lines.len() <= max_lines {
        lines
    } else {
        let remaining = lines.len() - max_lines;
        lines.truncate(max_lines);
        lines.push(mk_hint(remaining));
        lines
    }
}

/// Render a JSON object's entries as aligned key/value `Line`s, shared by tool args
/// and the result envelope's inner object.
///
/// Single-line values render inline (`  {key:<w}  {value}`, key dimmed and aligned to
/// the longest key capped at [`MAX_TOOL_KEY_WIDTH`]). Multi-line string values (those
/// containing `\n` — e.g. command stdout) expand into an indented block: a `key:`
/// header line followed by each line of the value indented under it. Compound values
/// use compact JSON; scalars use default formatting. Empty string values are skipped
/// (noise — e.g. an empty `stderr`). Output is bounded to `max_lines`.
///
/// `alert_keys` are boolean flags that are hidden when `false` (e.g. `timed_out`,
/// `truncated`) and rendered in red when `true` so a problem is visually prominent.
fn format_kv_lines(
    obj: &serde_json::Map<String, serde_json::Value>,
    max_lines: usize,
    alert_keys: &[&str],
) -> Vec<Line<'static>> {
    if obj.is_empty() {
        return Vec::new();
    }
    let key_w = obj
        .keys()
        .map(|k| k.len())
        .max()
        .unwrap_or(0)
        .min(MAX_TOOL_KEY_WIDTH);
    let key_style = Style::default().dim().fg(Color::Gray);
    let alert_style = Style::default().fg(Color::Red);
    let value_indent = " ".repeat(key_w + 4);

    let mut out: Vec<Line<'static>> = Vec::new();
    for (k, v) in obj {
        // Skip empty strings (e.g. an empty stderr) — pure noise.
        if let serde_json::Value::String(s) = v
            && s.is_empty()
        {
            continue;
        }
        // Alert flags: hide when false, render red when true. A non-bool value at
        // an alert key falls through to the normal path below.
        if alert_keys.contains(&k.as_str())
            && let serde_json::Value::Bool(b) = v
        {
            if *b {
                let key = format!("{:width$}", cap_chars(k, key_w), width = key_w);
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(key, alert_style),
                    Span::raw("  "),
                    Span::styled("true".to_owned(), alert_style),
                ]));
            }
            continue;
        }

        let value = tool_value_to_string(v);
        let key = format!("{:width$}", cap_chars(k, key_w), width = key_w);
        if value.contains('\n') {
            // Multi-line block: key with a trailing colon, then each value line
            // indented to the value column. No inline padding — the value lives
            // below, so the colon attaches directly to the key name.
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{}:", cap_chars(k, key_w)), key_style),
            ]));
            for line in value.split('\n') {
                out.push(Line::from(vec![
                    Span::raw(value_indent.clone()),
                    Span::raw(line.to_owned()),
                ]));
            }
        } else {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(key, key_style),
                Span::raw("  "),
                Span::raw(value),
            ]));
        }
    }
    bound_with_hint(out, max_lines, |n| {
        Line::from(vec![Span::raw("  "), Span::raw(more_lines_hint(n)).dim()])
    })
}

/// Pretty-print a raw JSON string and render each line as a two-space-indented
/// `Line` styled with `style`. Shared by the non-object fallbacks of tool args
/// (dim) and tool results (dim cyan).
fn indented_pretty_lines(raw: &str, max_lines: usize, style: Style) -> Vec<Line<'static>> {
    format_json_pretty_lines(raw, max_lines)
        .into_iter()
        .map(|l| Line::from(vec![Span::raw("  "), Span::styled(l, style)]))
        .collect()
}

/// Render a tool call's argument string. A JSON object is decomposed into aligned
/// key/value lines via [`format_kv_lines`]; any other shape (array / scalar / invalid
/// JSON) falls back to indented [`format_json_pretty_lines`].
fn format_tool_args(args: &str, max_lines: usize) -> Vec<Line<'static>> {
    match serde_json::from_str::<serde_json::Value>(args).ok() {
        Some(serde_json::Value::Object(obj)) => format_kv_lines(&obj, max_lines, &[]),
        _ => indented_pretty_lines(args, max_lines, Style::default().dim()),
    }
}

/// Header label for a tool result, derived from the result envelope.
///
/// Recognized envelopes (from `policy/executor.rs` / `approval.rs`):
/// - success `{"ok":true,"tool_name":...,"result":...}`     -> `▌result · {tool_name}`
/// - error   `{"ok":false,"tool_name":...,"error":...}`     -> `▌result · {tool_name} (failed)`
/// - deferred `{"ok":true,"pending_approval":true,...}`     -> `▌result · {tool_name} (pending)`
///
/// Anything else (e.g. the timeout string) -> `▌result`.
fn tool_result_header_label(result: &str) -> String {
    let obj = match serde_json::from_str::<serde_json::Value>(result).ok() {
        Some(serde_json::Value::Object(obj)) => obj,
        _ => return "\u{258C}result".to_owned(),
    };
    let tool_name = obj.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
    let suffix = if obj.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        " (failed)"
    } else if obj.get("pending_approval").and_then(|v| v.as_bool()) == Some(true) {
        " (pending)"
    } else {
        ""
    };
    if tool_name.is_empty() {
        "\u{258C}result".to_owned()
    } else {
        format!("\u{258C}result \u{00B7} {tool_name}{suffix}")
    }
}

/// Render the body of a tool result (everything below the header bar).
///
/// Unwraps the envelope: an error envelope expands its `error` text; a success/
/// deferred envelope renders its payload (`result` field if object/array, else the
/// envelope minus `ok`/`tool_name`) as key/value lines via [`format_kv_lines`], so
/// multi-line content like `stdout` is expanded into real line breaks. Non-envelope
/// input (the timeout string, non-JSON output) falls back to indented pretty-lines.
fn format_tool_result(result: &str, max_lines: usize) -> Vec<Line<'static>> {
    let obj = match serde_json::from_str::<serde_json::Value>(result).ok() {
        Some(serde_json::Value::Object(obj)) => obj,
        _ => {
            return indented_pretty_lines(
                result,
                max_lines,
                Style::default().dim().fg(Color::Cyan),
            );
        }
    };

    // Error envelope: surface the message, bounded + per-line capped like every
    // other body path (a giant stack trace must not blow past MAX_TOOL_RESULT_LINES).
    if obj.get("ok").and_then(|v| v.as_bool()) == Some(false)
        && let Some(err) = obj.get("error").and_then(|v| v.as_str())
    {
        let capped: Vec<String> = err
            .lines()
            .map(|l| truncate_chars(l, MAX_TOOL_LINE_CHARS))
            .collect();
        let style = Style::default().fg(Color::Red);
        return bound_with_hint(capped, max_lines, more_lines_hint)
            .into_iter()
            .map(|l| Line::from(vec![Span::raw("  "), Span::styled(l, style)]))
            .collect();
    }

    // Success / deferred: render the payload as key/value. Pick the inner `result`
    // when it is structured; otherwise kv-render the envelope minus bookkeeping keys
    // (`ok` / `tool_name`, already consumed by `tool_result_header_label`). The clone
    // is fine — envelopes are small and this runs once per cache miss.
    match obj.get("result") {
        Some(serde_json::Value::Object(inner)) => {
            format_kv_lines(inner, max_lines, TOOL_ALERT_KEYS)
        }
        Some(other) => {
            let raw = serde_json::to_string(other).unwrap_or_else(|_| other.to_string());
            indented_pretty_lines(&raw, max_lines, Style::default().dim().fg(Color::Cyan))
        }
        None => {
            let filtered: serde_json::Map<String, serde_json::Value> = obj
                .iter()
                .filter(|(k, _)| !matches!(k.as_str(), "ok" | "tool_name"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            format_kv_lines(&filtered, max_lines, TOOL_ALERT_KEYS)
        }
    }
}

/// Render a JSON value for the tool-args key/value view. Strings drop their quotes;
/// compound values use compact JSON bounded by [`MAX_TOOL_LINE_CHARS`]; scalars use
/// default formatting.
fn tool_value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => truncate_chars(
            &serde_json::to_string(v).unwrap_or_else(|_| v.to_string()),
            MAX_TOOL_LINE_CHARS,
        ),
        _ => v.to_string(),
    }
}

/// Truncate a string to at most `max` characters, appending "..." when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}...")
    }
}

/// Truncate `s` to at most `max` characters with no ellipsis — for fitting a column
/// or single row exactly (where "..." would itself overflow the budget).
fn cap_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// One styled `Line` per source line of `text`. Empty text yields no lines (a
/// zero-height entry is safe for the scroll math).
fn styled_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    text.lines()
        .map(|l| Line::styled(l.to_owned(), style))
        .collect()
}

/// One `Line` per source line of `text`; the first carries `prefix`, subsequent
/// lines carry a blank indent of `prefix.chars().count()` spaces so they align under
/// the first line's text. All spans share `style`. Empty text yields no lines.
fn prefixed_lines(prefix: &str, text: &str, style: Style) -> Vec<Line<'static>> {
    let indent = " ".repeat(prefix.chars().count());
    text.lines()
        .enumerate()
        .map(|(i, l)| {
            let lead = if i == 0 {
                prefix.to_owned()
            } else {
                indent.clone()
            };
            Line::styled(format!("{lead}{l}"), style)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use ratatui::style::Style;
    use ratatui::text::Line;

    use super::TOOL_ALERT_KEYS;
    use super::bound_with_hint;
    use super::cap_chars;
    use super::format_json_pretty_lines;
    use super::format_kv_lines;
    use super::format_tool_args;
    use super::format_tool_result;
    use super::more_lines_hint;
    use super::prefixed_lines;
    use super::styled_lines;
    use super::tool_result_header_label;
    use super::{MAX_TOOL_ARG_LINES, MAX_TOOL_LINE_CHARS, TOOL_HEADER_BG};
    use crate::tui::{App, ChatLine};

    fn app_with(lines: Vec<ChatLine>) -> App {
        let mut app = App::new();
        app.chat_lines = lines;
        app
    }

    /// Flatten a `Line`'s spans into their concatenated text.
    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn pretty_json_object_is_indented_multiline() {
        let lines = format_json_pretty_lines(r#"{"a":1,"b":2}"#, MAX_TOOL_ARG_LINES);
        assert!(lines.len() >= 4, "got {lines:?}");
        assert_eq!(lines.first().map(String::as_str), Some("{"));
        // Inner lines are indented.
        assert!(
            lines[1..lines.len() - 1]
                .iter()
                .any(|l| l.starts_with("  "))
        );
    }

    #[test]
    fn pretty_json_array() {
        let lines = format_json_pretty_lines("[1,2,3]", MAX_TOOL_ARG_LINES);
        assert!(lines.len() >= 2);
        assert_eq!(lines.first().map(String::as_str), Some("["));
    }

    #[test]
    fn pretty_json_invalid_falls_back_to_raw() {
        let lines = format_json_pretty_lines("not json\nline two", MAX_TOOL_ARG_LINES);
        assert_eq!(lines, vec!["not json".to_string(), "line two".to_string()]);
    }

    #[test]
    fn pretty_json_truncates_with_hint() {
        // 15 keys pretty-print to 17 lines ({, 15 entries, }).
        let keys: Vec<String> = (0..15).map(|i| format!("\"k{i}\": {i}")).collect();
        let json = format!("{{{}}}", keys.join(", "));
        let lines = format_json_pretty_lines(&json, 5);
        assert_eq!(lines.len(), 6, "5 kept + 1 hint");
        assert!(lines.last().unwrap().contains("more lines"));
        // The hint reports the exact remainder.
        assert!(lines.last().unwrap().contains("12 more lines"));
    }

    #[test]
    fn pretty_json_caps_long_line() {
        // A single huge JSON string scalar: one line, capped to MAX_TOOL_LINE_CHARS.
        let raw = format!(r#""{}""#, "x".repeat(10_000));
        let lines = format_json_pretty_lines(&raw, MAX_TOOL_ARG_LINES);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].ends_with("..."));
        assert!(lines[0].chars().count() <= MAX_TOOL_LINE_CHARS + 3);
    }

    #[test]
    fn cache_hit_skips_rerender() {
        let mut app = app_with(vec![ChatLine::Assistant("# hi\n\ntext".into())]);
        let (text_a, total_a, _) = app.build_chat_text(80);
        assert!(app.render_cache[0].is_some(), "first build populates cache");
        let (text_b, total_b, _) = app.build_chat_text(80);
        assert_eq!(total_a, total_b);
        assert_eq!(text_a.lines.len(), text_b.lines.len());
        assert_eq!(app.render_cache[0].as_ref().unwrap().width, 78);
    }

    #[test]
    fn cache_rebuilds_on_width_change() {
        let mut app = app_with(vec![ChatLine::Assistant("some text here".into())]);
        let _ = app.build_chat_text(80);
        let w0 = app.render_cache[0].as_ref().unwrap().width;
        let _ = app.build_chat_text(100);
        let w1 = app.render_cache[0].as_ref().unwrap().width;
        assert_ne!(w0, w1);
        assert_eq!(w1, 98);
    }

    #[test]
    fn streaming_delta_invalidates_only_last() {
        let mut app = app_with(vec![
            ChatLine::Assistant("first".into()),
            ChatLine::Assistant("second".into()),
        ]);
        let _ = app.build_chat_text(80);
        assert!(app.render_cache[0].is_some());
        assert!(app.render_cache[1].is_some());

        app.append_streaming_delta(true, " more");
        assert!(
            app.render_cache[0].is_some(),
            "prior entry cache must be untouched"
        );
        assert!(
            app.render_cache[1].is_none(),
            "streaming entry cache must be invalidated"
        );
    }

    #[test]
    fn clear_chat_drops_cache() {
        let mut app = app_with(vec![ChatLine::Assistant("x".into())]);
        let _ = app.build_chat_text(80);
        assert!(!app.render_cache.is_empty());
        app.clear_chat();
        assert!(app.render_cache.is_empty());
        assert!(app.chat_lines.is_empty());
    }

    #[test]
    fn format_tool_args_object_renders_key_value() {
        let lines = format_tool_args(r#"{"path":"/foo/bar","offset":10}"#, MAX_TOOL_ARG_LINES);
        assert_eq!(lines.len(), 2);
        let joined: Vec<String> = lines.iter().map(line_text).collect();
        // String value renders without quotes.
        assert!(
            joined
                .iter()
                .any(|s| s.contains("path") && s.contains("/foo/bar"))
        );
        assert!(
            joined
                .iter()
                .any(|s| s.contains("offset") && s.contains("10"))
        );
        // No raw JSON braces leak into the key/value view.
        assert!(!joined.iter().any(|s| s.contains('{')));
    }

    #[test]
    fn format_tool_args_nested_value_is_compact() {
        let lines = format_tool_args(r#"{"a":{"x":1}}"#, MAX_TOOL_ARG_LINES);
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains(r#""x":1"#));
    }

    #[test]
    fn format_tool_args_non_object_falls_back() {
        let lines = format_tool_args("[1,2,3]", MAX_TOOL_ARG_LINES);
        let joined: Vec<String> = lines.iter().map(line_text).collect();
        assert!(joined.iter().any(|s| s.contains('[')));
    }

    #[test]
    fn format_tool_args_empty_object_no_body() {
        assert!(format_tool_args("{}", MAX_TOOL_ARG_LINES).is_empty());
    }

    #[test]
    fn format_tool_args_truncates_with_hint() {
        let keys: Vec<String> = (0..15).map(|i| format!("\"k{i}\": {i}")).collect();
        let json = format!("{{{}}}", keys.join(", "));
        let lines = format_tool_args(&json, 5);
        assert_eq!(lines.len(), 6, "5 kept + 1 hint");
        assert!(line_text(lines.last().unwrap()).contains("more lines"));
    }

    #[test]
    fn styled_lines_splits_and_empties() {
        let s = Style::default();
        assert_eq!(styled_lines("", s).len(), 0);
        assert_eq!(styled_lines("a\nb", s).len(), 2);
    }

    #[test]
    fn prefixed_lines_first_then_indent() {
        let lines = prefixed_lines("[error] ", "boom\nbang", Style::default());
        assert_eq!(lines.len(), 2);
        assert_eq!(line_text(&lines[0]), "[error] boom");
        // Continuation indented by the prefix width (8 ASCII spaces).
        assert_eq!(line_text(&lines[1]), format!("{}bang", " ".repeat(8)));
        assert!(prefixed_lines("[error] ", "", Style::default()).is_empty());
    }

    #[test]
    fn bound_with_hint_under_and_over() {
        assert_eq!(
            bound_with_hint(vec!["a".to_string()], 5, more_lines_hint),
            vec!["a".to_string()]
        );
        let out = bound_with_hint(
            vec!["a".to_string(), "b".into(), "c".into()],
            2,
            more_lines_hint,
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out[2], "... (1 more lines)");
    }

    #[test]
    fn format_kv_lines_inline_value() {
        let obj = serde_json::from_str::<serde_json::Value>(r#"{"path":"/x","n":3}"#)
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        let lines = format_kv_lines(&obj, 50, &[]);
        assert_eq!(lines.len(), 2);
        let joined: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            joined
                .iter()
                .any(|s| s.contains("path") && s.contains("/x"))
        );
        assert!(joined.iter().any(|s| s.contains("n") && s.contains("3")));
    }

    #[test]
    fn format_kv_lines_multiline_value_expands() {
        // Raw string: serde_json turns `\n` into a real newline in the value.
        let obj = serde_json::from_str::<serde_json::Value>(r#"{"stdout":"a\nb\nc","ok":1}"#)
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        let lines = format_kv_lines(&obj, 50, &[]);
        let joined: Vec<String> = lines.iter().map(line_text).collect();
        // The key line carries a trailing colon and no inline value.
        assert!(joined.iter().any(|s| s.contains("stdout:")));
        // Each content line appears on its own row, not as a literal "\n".
        assert!(joined.iter().any(|s| s.ends_with("a")));
        assert!(joined.iter().any(|s| s.ends_with("b")));
        assert!(joined.iter().any(|s| s.ends_with("c")));
        assert!(!joined.iter().any(|s| s.contains(r"\n")));
    }

    #[test]
    fn format_kv_lines_skips_empty_strings() {
        let obj = serde_json::from_str::<serde_json::Value>(r#"{"stdout":"ok","stderr":""}"#)
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        let joined: Vec<String> = format_kv_lines(&obj, 50, &[])
            .iter()
            .map(line_text)
            .collect();
        assert!(joined.iter().any(|s| s.contains("stdout")));
        // Empty stderr is suppressed — it's noise.
        assert!(!joined.iter().any(|s| s.contains("stderr")));
    }

    #[test]
    fn format_kv_lines_alert_keys_hide_false_flag_true_red() {
        // timed_out=false is hidden; truncated=true is shown. Both are in TOOL_ALERT_KEYS.
        let obj = serde_json::from_str::<serde_json::Value>(
            r#"{"exit_code":0,"timed_out":false,"truncated":true}"#,
        )
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
        let lines = format_kv_lines(&obj, 50, TOOL_ALERT_KEYS);
        let joined: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            !joined.iter().any(|s| s.contains("timed_out")),
            "false alert hidden"
        );
        assert!(
            joined
                .iter()
                .any(|s| s.contains("truncated") && s.contains("true")),
            "true alert shown"
        );
        // The true alert span carries the red foreground.
        let truncated_line = lines
            .iter()
            .find(|l| line_text(l).contains("truncated"))
            .unwrap();
        assert!(
            truncated_line
                .spans
                .iter()
                .any(|s| { s.style.fg == Some(ratatui::style::Color::Red) })
        );
    }

    #[test]
    fn format_tool_result_success_decomposes_envelope() {
        let result = r#"{"ok":true,"tool_name":"bash_exec","result":{"stdout":"hello\nworld","exit_code":0}}"#;
        let body = format_tool_result(result, 50);
        let joined: Vec<String> = body.iter().map(line_text).collect();
        // Inner fields are rendered as key/value, not envelope braces.
        assert!(joined.iter().any(|s| s.contains("stdout:")));
        assert!(joined.iter().any(|s| s.contains("exit_code")));
        // stdout content is expanded into real lines, not a literal "\n".
        assert!(joined.iter().any(|s| s.ends_with("hello")));
        assert!(joined.iter().any(|s| s.ends_with("world")));
        assert!(!joined.iter().any(|s| s.contains(r"\n")));
        assert!(!joined.iter().any(|s| s.contains("tool_name")));
    }

    #[test]
    fn format_tool_result_error_surfaces_message() {
        let result = r#"{"ok":false,"tool_name":"bash_exec","error":"command not found"}"#;
        let body = format_tool_result(result, 50);
        let joined: Vec<String> = body.iter().map(line_text).collect();
        assert!(joined.iter().any(|s| s.contains("command not found")));
    }

    #[test]
    fn format_tool_result_non_envelope_falls_back() {
        let body = format_tool_result("tool 'bash_exec' timed out after 120s", 50);
        let joined: Vec<String> = body.iter().map(line_text).collect();
        assert!(joined.iter().any(|s| s.contains("timed out")));
    }

    #[test]
    fn tool_result_header_label_variants() {
        let ok = r#"{"ok":true,"tool_name":"bash_exec","result":{}}"#;
        assert!(tool_result_header_label(ok).contains("bash_exec"));
        assert!(!tool_result_header_label(ok).contains("failed"));
        let err = r#"{"ok":false,"tool_name":"bash_exec","error":"x"}"#;
        assert!(tool_result_header_label(err).contains("(failed)"));
        let pending = r#"{"ok":true,"pending_approval":true,"tool_name":"bash_exec","id":"ap_"}"#;
        assert!(tool_result_header_label(pending).contains("(pending)"));
        assert_eq!(tool_result_header_label("not json"), "\u{258C}result");
    }

    #[test]
    fn header_marks_track_tool_rows() {
        let mut app = app_with(vec![
            ChatLine::Assistant("hello\nworld".into()),
            ChatLine::ToolCall {
                name: "ls".into(),
                args: "{}".into(),
            },
        ]);
        let (_, _, header_marks) = app.build_chat_text(80);
        assert_eq!(header_marks.len(), 1);
        let (row, bg) = header_marks[0];
        assert_eq!(bg, TOOL_HEADER_BG);
        // The header sits at the cumulative wrapped height of prior entries.
        let prior = app.render_cache[0].as_ref().unwrap().wrapped_height;
        assert_eq!(row, prior);
    }

    #[test]
    fn cap_chars_hard_truncates() {
        assert_eq!(cap_chars("abc", 5), "abc");
        assert_eq!(cap_chars("abcdef", 3), "abc");
    }

    #[test]
    fn format_tool_args_long_key_is_capped() {
        // A key longer than MAX_TOOL_KEY_WIDTH is hard-cut so the value column
        // stays aligned with shorter keys.
        let json = format!(r#"{{"{}":1, "k":2}}"#, "x".repeat(40));
        let lines = format_tool_args(&json, MAX_TOOL_ARG_LINES);
        assert_eq!(lines.len(), 2);
        // Both key spans occupy the same column width.
        let key_widths: Vec<usize> = lines.iter().map(|l| l.spans[1].content.len()).collect();
        assert_eq!(key_widths[0], key_widths[1]);
    }
}
