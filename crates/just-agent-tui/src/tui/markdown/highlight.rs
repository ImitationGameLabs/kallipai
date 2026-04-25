//! Syntect-based code highlighting for the markdown renderer.

use std::sync::LazyLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use tracing::warn;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// Highlight code using syntect. Returns lines of ratatui Spans directly.
pub fn highlight_code(code: &str, lang: Option<&str>) -> Vec<Vec<Span<'static>>> {
    let t0 = std::time::Instant::now();

    let ss = &*SYNTAX_SET;
    let theme = &THEME_SET.themes["base16-eighties.dark"];

    let syntax = lang
        .and_then(|l| ss.find_syntax_by_token(l))
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let mut highlighter = syntect::easy::HighlightLines::new(syntax, theme);
    let mut result = Vec::new();

    for line in code.lines() {
        let ranges = highlighter.highlight_line(line, ss).unwrap_or_default();
        let spans: Vec<Span<'static>> = ranges
            .into_iter()
            .map(|(style, text)| Span::styled(text.to_owned(), syntect_style_to_ratatui(style)))
            .collect();
        result.push(if spans.is_empty() {
            vec![Span::styled("", Style::default())]
        } else {
            spans
        });
    }

    let total = t0.elapsed();
    if total.as_millis() > 2 {
        warn!("highlight_code: {}ms lang={:?}", total.as_millis(), lang);
    }

    result
}

fn syntect_style_to_ratatui(style: syntect::highlighting::Style) -> Style {
    let fg = syntect_color_to_ratatui(style.foreground);
    let mut s = Style::default().fg(fg);
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::BOLD)
    {
        s = s.add_modifier(Modifier::BOLD);
    }
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::ITALIC)
    {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::UNDERLINE)
    {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    s
}

fn syntect_color_to_ratatui(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}
