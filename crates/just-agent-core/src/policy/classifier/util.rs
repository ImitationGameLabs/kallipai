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

pub(super) fn is_literal_part(node: &Node) -> bool {
    matches!(&node.kind, rable::NodeKind::WordLiteral { .. })
}

pub(super) fn extract_command_name(word: &Node) -> Option<&str> {
    word_literal_value(word)
}

pub(super) fn has_any_flag(words: &[Node], flags: &[&str]) -> bool {
    words
        .iter()
        .skip(1)
        .any(|w| word_literal_value(w).is_some_and(|v| flags.contains(&v)))
}

pub(super) fn has_subcommand_and_flag(words: &[Node], subcmd: &str, flag: &str) -> bool {
    words.len() >= 3
        && word_literal_value(&words[1]).is_some_and(|v| v == subcmd)
        && words
            .iter()
            .skip(2)
            .any(|w| word_literal_value(w).is_some_and(|v| v == flag))
}
