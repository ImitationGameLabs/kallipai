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

/// Return the first literal flag in `words` (after the command name) that is in
/// `flags`, comparing the unquoted form. Used by catalog constraints both to
/// *trip* and to *name* the offending flag in the defer reason.
pub(super) fn find_mutating_flag<'a>(words: &[Node], flags: &[&'a str]) -> Option<&'a str> {
    words.iter().skip(1).find_map(|w| {
        let v = word_literal_value(w).map(strip_surrounding_quotes);
        flags.iter().copied().find(|f| Some(*f) == v)
    })
}

/// The literal text of a redirect target word, with one layer of surrounding
/// quotes stripped. Returns `None` for words containing expansions (e.g.
/// `>$(cmd)` or `>$x`) — those targets cannot be statically reasoned about.
///
/// Single helper for both the `/dev/null` sink check and the redirect reason
/// wording, so `> 'file'` is reported as `file` and matched consistently.
pub(super) fn redirect_target_literal(node: &Node) -> Option<&str> {
    word_literal_value(node).map(strip_surrounding_quotes)
}

/// Strip one layer of surrounding matching quotes from a literal word value.
///
/// `rable` keeps quotes in `Word.value` (e.g. `'-delete'`), so flag/target
/// matching must compare the unquoted form — otherwise a quoted mutating flag
/// (`find '-delete'`) evades its constraint and auto-runs.
pub(super) fn strip_surrounding_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if s.len() >= 2 {
        let (first, last) = (bytes[0], bytes[s.len() - 1]);
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return &s[1..s.len() - 1];
        }
    }
    s
}
