//! Low-level helpers for inspecting rable AST word nodes.

use rable::Node;

/// Extract the text value of a Word node if it contains only literal parts.
///
/// rable always produces at least one part per Word (a `WordLiteral`), even
/// for plain text like `"cargo"`. Returns `None` for words containing
/// expansions (CommandSubstitution, ParamExpansion, etc.).
pub(super) fn word_literal_value(word: &Node) -> Option<&str> {
    match &word.kind {
        rable::NodeKind::Word { value, parts, .. } if parts.iter().all(is_literal_part) => {
            Some(value)
        }
        _ => None,
    }
}

fn is_literal_part(node: &Node) -> bool {
    matches!(&node.kind, rable::NodeKind::WordLiteral { .. })
}

pub(super) fn extract_command_name(word: &Node) -> Option<&str> {
    word_literal_value(word)
}

pub(super) fn has_any_flag(words: &[Node], flags: &[&str]) -> bool {
    words.iter().skip(1).any(|w| {
        word_literal_value(w)
            .map(strip_surrounding_quotes)
            .is_some_and(|v| flags.contains(&v))
    })
}

/// Strip one layer of surrounding matching quotes from a literal word value.
///
/// `rable` keeps quotes in `Word.value` (e.g. `'-delete'`), so flag matching must
/// compare the unquoted form — otherwise a quoted mutating flag (`find '-delete'`)
/// evades its constraint and auto-runs.
fn strip_surrounding_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if s.len() >= 2 {
        let (first, last) = (bytes[0], bytes[s.len() - 1]);
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return &s[1..s.len() - 1];
        }
    }
    s
}
