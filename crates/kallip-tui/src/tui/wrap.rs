//! Word-wrap line counting using ratatui's own `WordWrapper`.

use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};

/// Count visual lines after word wrapping using ratatui's actual `WordWrapper`.
pub fn word_wrap_line_count(text: &Text, width: usize) -> usize {
    if width == 0 {
        return text.lines.len();
    }
    let paragraph = Paragraph::new(text.clone()).wrap(Wrap { trim: true });
    paragraph.line_count(width as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_count_simple() {
        let text = Text::from("Hello world, this is a test of word wrapping behavior.");
        assert_eq!(word_wrap_line_count(&text, 10), 7);
        assert_eq!(word_wrap_line_count(&text, 20), 3);
        assert_eq!(word_wrap_line_count(&text, 80), 1);
    }

    #[test]
    fn wrap_count_empty() {
        let text = Text::from("");
        assert_eq!(word_wrap_line_count(&text, 80), 1);
    }

    #[test]
    fn wrap_count_zero_width() {
        let text = Text::from("Hello");
        assert_eq!(word_wrap_line_count(&text, 0), 1);
    }
}
