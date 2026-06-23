//! Shell interpreter delegation — re-parse and classify inner commands.
//!
//! rable treats `bash -c "sudo rm -rf /"` as a Command with opaque string
//! arguments — the inner command is NOT recursively parsed. This module
//! detects shell interpreters and eval-like commands, strips quotes from the
//! string argument, re-parses it with rable, and classifies the inner AST.

use rable::Node;

use super::Safety;
use super::catalog::{self, CommandSpec};
use super::{util, walker};

pub(super) fn classify_interpreter_delegate(
    catalog: &'static [CommandSpec],
    cmd_name: &str,
    words: &[Node],
) -> Option<Safety> {
    let inner_source = if catalog::SHELL_INTERPRETERS.contains(&cmd_name) {
        find_c_flag_argument(words)?
    } else if catalog::EVAL_COMMANDS.contains(&cmd_name) {
        collect_remaining_args(words)?
    } else {
        return None;
    };

    let inner_source = strip_quotes(&inner_source);

    match rable::parse(&inner_source, false) {
        Ok(nodes) => Some(walker::classify_nodes(catalog, &nodes)),
        Err(_) => Some(Safety::Reject {
            reason: "failed to parse inner command for interpreter delegate".into(),
        }),
    }
}

fn strip_quotes(s: &str) -> String {
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        let last = s.as_bytes()[s.len() - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return s[1..s.len() - 1].to_owned();
        }
    }
    s.to_owned()
}

fn find_c_flag_argument(words: &[Node]) -> Option<String> {
    let mut found_c = false;
    for word in &words[1..] {
        let val = util::word_literal_value(word)?;
        if found_c {
            return Some(val.to_owned());
        }
        if catalog::COMMAND_STRING_FLAGS.contains(&val) {
            found_c = true;
        }
    }
    None
}

/// Collect all remaining literal args, strip quotes per-word, and join with spaces.
fn collect_remaining_args(words: &[Node]) -> Option<String> {
    let args: Vec<String> = words[1..]
        .iter()
        .filter_map(|w| util::word_literal_value(w).map(strip_quotes))
        .collect();
    if args.is_empty() {
        None
    } else {
        Some(args.join(" "))
    }
}
