//! Shared helpers for shell backend implementations.

/// Extracts new lines produced by a shell command.
///
/// The scrollback buffer is a line-by-line history of all terminal output
/// (command echo, program stdout/stderr, shell prompts) accumulated since the
/// session started.
///
/// By taking a snapshot *before* the command is sent and another *after* it
/// finishes, this function strips the shared prefix — i.e. everything that was
/// already on screen — and returns only the lines that appeared in between,
/// which correspond to the command's own output.
pub(crate) fn strip_common_prefix(before: &str, after: &str) -> String {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let common = before_lines
        .iter()
        .zip(after_lines.iter())
        .take_while(|(a, b)| a == b)
        .count();
    after_lines[common..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_prefix_returns_only_new_content() {
        let before = "line1\nline2\nline3";
        let after = "line1\nline2\nline3\nnew output\nmore";
        assert_eq!(strip_common_prefix(before, after), "new output\nmore");
    }

    #[test]
    fn strip_prefix_returns_everything_when_unrelated() {
        assert_eq!(strip_common_prefix("abc", "def"), "def");
    }

    #[test]
    fn strip_prefix_handles_empty_before() {
        assert_eq!(strip_common_prefix("", "hello"), "hello");
    }
}
