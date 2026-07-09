//! Markdown-to-ratatui renderer with syntect-based code highlighting.

mod highlight;

use std::time::Instant;

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use tracing::warn;

/// Render a markdown string into styled ratatui Lines.
///
/// When `highlight` is false, fenced code blocks render as plain monospace
/// spans instead of running syntect. Used for the in-flight streaming entry,
/// where re-highlighting a growing code block on every token dominates CPU; the
/// finalized entry is re-rendered with `highlight = true` once streaming ends
/// (see `App::finalize_streaming`).
pub fn render_markdown(input: &str, term_width: u16, highlight: bool) -> Vec<Line<'static>> {
    use pulldown_cmark::{Options, Parser};

    let t0 = Instant::now();

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(input, opts);
    let mut renderer = MdRenderer {
        term_width,
        highlight,
        ..Default::default()
    };
    renderer.run(parser);

    let elapsed = t0.elapsed();
    if elapsed.as_millis() > 5 {
        warn!(
            "render_markdown took {}ms ({} lines output)",
            elapsed.as_millis(),
            renderer.lines.len()
        );
    }

    renderer.lines
}

/// Stateful markdown-to-ratatui renderer.
#[derive(Default)]
struct MdRenderer {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    term_width: u16,
    /// When false, code blocks skip syntect highlighting (plain monospace).
    highlight: bool,
    list_depth: usize,
    list_counters: Vec<u64>,
    in_code_block: bool,
    code_block_lines: Vec<String>,
    code_block_lang: Option<String>,
    in_table: bool,
    table_rows: Vec<Vec<String>>,
    table_current_row: Vec<String>,
    table_current_cell: String,
    heading_level: Option<u8>,
    blockquote_depth: usize,
}

impl MdRenderer {
    fn current_style(&self) -> Style {
        let mut style = Style::default();
        for &s in &self.style_stack {
            style = style.patch(s);
        }
        style
    }

    fn push_span(&mut self, text: impl Into<String>) {
        let style = self.current_style();
        let text = text.into();
        if text.is_empty() {
            return;
        }

        // Apply blockquote prefix
        if self.blockquote_depth > 0 && self.lines.is_empty() && self.current.is_empty() {
            let prefix = "│ ".repeat(self.blockquote_depth);
            self.current
                .push(Span::styled(prefix, Style::default().dim()));
        }

        self.current.push(Span::styled(text, style));
    }

    fn finish_line(&mut self) {
        let line = std::mem::take(&mut self.current);
        self.lines.push(Line::from(line));
    }

    fn run<'b>(&mut self, events: impl Iterator<Item = pulldown_cmark::Event<'b>>) {
        use pulldown_cmark::Event;

        for event in events {
            match event {
                Event::Start(tag) => self.handle_start(&tag),
                Event::End(tag) => self.handle_end(tag),
                Event::Text(text) => {
                    if self.in_code_block {
                        self.code_block_lines.push(text.into_string());
                    } else if self.in_table {
                        self.table_current_cell.push_str(&text);
                    } else {
                        self.push_span(text.into_string());
                    }
                }
                Event::Code(code) => {
                    let style = Style::default().fg(Color::Yellow);
                    self.current.push(Span::styled(format!("`{code}`"), style));
                }
                Event::SoftBreak => {
                    if !self.in_table {
                        self.finish_line();
                    }
                }
                Event::HardBreak => {
                    if !self.in_table {
                        self.finish_line();
                    }
                }
                Event::Rule => {
                    self.finish_line();
                    self.lines
                        .push(Line::from("─".repeat(40).dim().fg(Color::DarkGray)));
                }
                Event::Html(html) => {
                    self.push_span(html.into_string());
                }
                Event::InlineHtml(html) => {
                    self.push_span(html.into_string());
                }
                Event::TaskListMarker(checked) => {
                    let marker = if checked { "[x] " } else { "[ ] " };
                    self.push_span(marker);
                }
                _ => {}
            }
        }

        // Flush remaining
        if !self.current.is_empty() {
            self.finish_line();
        }
    }

    fn handle_start(&mut self, tag: &pulldown_cmark::Tag<'_>) {
        use pulldown_cmark::Tag;

        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.heading_level = Some(*level as u8);
                let mut style = Style::default().bold();
                match level {
                    pulldown_cmark::HeadingLevel::H1 | pulldown_cmark::HeadingLevel::H2 => {
                        style = style.fg(Color::Cyan);
                    }
                    _ => style = style.fg(Color::Blue),
                }
                self.style_stack.push(style);
            }
            Tag::BlockQuote(_) => {
                self.blockquote_depth += 1;
                self.style_stack.push(Style::default().dim());
            }
            Tag::CodeBlock(kind) => {
                self.in_code_block = true;
                self.code_block_lines.clear();
                self.code_block_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                        Some(lang.to_string())
                    }
                    _ => None,
                };
                let lang_label = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                        format!("╭─ {lang}")
                    }
                    _ => "╭─".to_owned(),
                };
                self.lines.push(Line::from(lang_label.fg(Color::Gray)));
            }
            Tag::List(start_number) => {
                self.list_depth += 1;
                self.list_counters.push(start_number.unwrap_or(0));
            }
            Tag::Item => {
                if !self.current.is_empty() {
                    self.finish_line();
                }
                let indent = "  ".repeat(self.list_depth.saturating_sub(1));
                let bullet = match self.list_counters.last() {
                    Some(0) | None => "- ".to_owned(),
                    Some(n) => format!("{n}. "),
                };
                self.push_span(format!("{indent}{bullet}"));
                if let Some(counter) = self.list_counters.last_mut()
                    && *counter != 0
                {
                    *counter += 1;
                }
            }
            Tag::Strong => {
                self.style_stack
                    .push(Style::default().add_modifier(Modifier::BOLD));
            }
            Tag::Emphasis => {
                self.style_stack
                    .push(Style::default().add_modifier(Modifier::ITALIC));
            }
            Tag::Strikethrough => {
                self.style_stack
                    .push(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { .. } => {
                self.style_stack.push(Style::default().fg(Color::Blue));
            }
            Tag::Table(_) => {
                self.in_table = true;
                self.table_rows.clear();
            }
            Tag::TableHead => {
                self.table_current_row = Vec::new();
                self.table_current_cell = String::new();
            }
            Tag::TableRow => {
                self.table_current_row = Vec::new();
                self.table_current_cell = String::new();
            }
            Tag::TableCell => {
                self.table_current_cell = String::new();
            }
            _ => {}
        }
    }

    fn handle_end(&mut self, tag: pulldown_cmark::TagEnd) {
        use pulldown_cmark::TagEnd;

        match tag {
            TagEnd::Paragraph => {
                self.finish_line();
            }
            TagEnd::Heading(_) => {
                self.finish_line();
                self.heading_level = None;
                self.style_stack.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.finish_line();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.style_stack.pop();
            }
            TagEnd::CodeBlock => {
                self.render_code_block();
                self.in_code_block = false;
                self.code_block_lines.clear();
                self.code_block_lang = None;
            }
            TagEnd::List(_) => {
                self.list_depth = self.list_depth.saturating_sub(1);
                self.list_counters.pop();
            }
            TagEnd::Item => {
                if !self.current.is_empty() {
                    self.finish_line();
                }
            }
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                self.style_stack.pop();
            }
            TagEnd::Link => {
                self.style_stack.pop();
            }
            TagEnd::Table => {
                self.in_table = false;
                self.render_table();
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                self.table_rows
                    .push(std::mem::take(&mut self.table_current_row));
            }
            TagEnd::TableCell => {
                self.table_current_row
                    .push(std::mem::take(&mut self.table_current_cell));
            }
            _ => {}
        }
    }

    /// Render collected code lines, with syntect highlighting when enabled.
    ///
    /// When `highlight` is false (the in-flight streaming tail), code renders as
    /// plain monospace instead of running syntect. Both paths iterate
    /// `code.lines()` and emit one `│ `-prefixed line per source line, so the
    /// deferred and highlighted forms produce the same row count — the cache key
    /// does not distinguish them, and the finalize transition does not shift
    /// scroll math.
    fn render_code_block(&mut self) {
        let code: String = self.code_block_lines.join("");
        let prefix_style = Style::default().fg(Color::DarkGray);

        // Normalize both paths to `Vec<Vec<Span>>` so the framing loop is shared
        // and row-count parity is structural rather than comment-enforced.
        let body: Vec<Vec<Span<'static>>> = if self.highlight {
            highlight::highlight_code(&code, self.code_block_lang.as_deref())
        } else {
            let code_style = Style::default().fg(Color::Gray);
            code.lines()
                .map(|line| vec![Span::styled(line.to_owned(), code_style)])
                .collect()
        };
        for span_line in body {
            let mut spans = vec![Span::styled("│ ", prefix_style)];
            spans.extend(span_line);
            self.lines.push(Line::from(spans));
        }
        self.lines
            .push(Line::from("╰─".to_owned().fg(Color::DarkGray)));
    }

    /// Render collected table rows using comfy-table.
    fn render_table(&mut self) {
        use comfy_table::Table;
        use comfy_table::presets::UTF8_FULL;

        if self.table_rows.is_empty() {
            return;
        }

        let width = if self.term_width > 4 {
            self.term_width - 2
        } else {
            80
        };

        let mut table = Table::new();
        table.load_preset(UTF8_FULL).force_no_tty().set_width(width);

        let mut rows_iter = self.table_rows.iter();
        if let Some(header) = rows_iter.next() {
            table.set_header(header);
        }
        for row in rows_iter {
            table.add_row(row);
        }

        let style = Style::default().dim().fg(Color::Gray);
        for line in table.lines() {
            self.lines.push(Line::from(Span::styled(line, style)));
        }
    }
}
