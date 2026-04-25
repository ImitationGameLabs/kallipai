//! Helper functions for the AST walker — decision merging, redirect/assignment
//! classification, and pipeline pattern detection.

use rable::{Node, NodeKind};

use super::super::ToolDecision;
use super::{lists, util};

/// Return the stricter of two decisions: Deny > Ask > Allow.
pub(super) fn stricter(a: ToolDecision, b: ToolDecision) -> ToolDecision {
    match (a, b) {
        (ToolDecision::Deny { reason }, _) | (_, ToolDecision::Deny { reason }) => {
            ToolDecision::Deny { reason }
        }
        (
            ToolDecision::Ask { reason: ra, dangerous: da },
            ToolDecision::Ask { reason: rb, dangerous: db },
        ) => {
            let reason = if ra.len() >= rb.len() { ra } else { rb };
            ToolDecision::Ask { reason, dangerous: da || db }
        }
        (ToolDecision::Ask { reason, dangerous }, ToolDecision::Allow)
        | (ToolDecision::Allow, ToolDecision::Ask { reason, dangerous }) => {
            ToolDecision::Ask { reason, dangerous }
        }
        (ToolDecision::Allow, ToolDecision::Allow) => ToolDecision::Allow,
    }
}

/// Merge decisions from required nodes, an optional node, and redirects.
pub(super) fn classify_multi(
    required: &[&Node],
    optional: Option<&Node>,
    redirects: &[Node],
) -> ToolDecision {
    let mut dec = required
        .iter()
        .map(|n| super::walker::classify_node_ref(n))
        .fold(ToolDecision::Allow, stricter);
    if let Some(opt) = optional {
        dec = stricter(dec, super::walker::classify_node_ref(opt));
    }
    stricter(dec, classify_redirects(redirects))
}

// ---------------------------------------------------------------------------
// Redirect classification
// ---------------------------------------------------------------------------

pub(super) fn classify_redirects(redirects: &[Node]) -> ToolDecision {
    redirects
        .iter()
        .map(classify_redirect_node)
        .fold(ToolDecision::Allow, stricter)
}

pub(super) fn classify_redirect_node(node: &Node) -> ToolDecision {
    match &node.kind {
        NodeKind::Redirect { op, target, .. } => {
            let op_dec = match op.as_str() {
                "<" => ToolDecision::Allow,
                ">" | ">>" | ">|" | "<>" | "&>" | "&>>" => ToolDecision::Ask {
                    reason: "shell redirection may modify files".into(),
                    dangerous: false,
                },
                _ => ToolDecision::Ask {
                    reason: format!("redirect operator '{op}' requires approval"),
                    dangerous: false,
                },
            };
            stricter(op_dec, super::walker::classify_node_ref(target))
        }
        NodeKind::HereDoc { .. } => {
            ToolDecision::Ask { reason: "heredocs require approval".into(), dangerous: false }
        }
        _ => super::walker::classify_node_ref(node),
    }
}

// ---------------------------------------------------------------------------
// Assignment classification
// ---------------------------------------------------------------------------

/// Classify variable assignments preceding a command.
///
/// Checks both the variable name (dangerous env vars like PATH) and the
/// value parts (which may contain command substitutions).
pub(super) fn classify_assignments(assignments: &[Node]) -> ToolDecision {
    assignments
        .iter()
        .map(|a| {
            let name_dec = check_assignment_name(a);
            let parts_dec = match &a.kind {
                NodeKind::Word { parts, .. } if !parts.is_empty() => classify_word_parts(parts),
                _ => ToolDecision::Allow,
            };
            stricter(name_dec, parts_dec)
        })
        .fold(ToolDecision::Allow, stricter)
}

fn check_assignment_name(assignment: &Node) -> ToolDecision {
    let name = match &assignment.kind {
        NodeKind::Word { value, .. } => value.split('=').next().unwrap_or(""),
        _ => return ToolDecision::Allow,
    };
    if lists::DANGEROUS_ENV_VARS
        .iter()
        .any(|v| v.eq_ignore_ascii_case(name))
    {
        return ToolDecision::Ask {
            reason: format!("assignment to '{name}' can affect security-critical behavior"),
            dangerous: false,
        };
    }
    ToolDecision::Allow
}

// ---------------------------------------------------------------------------
// Word expansion classification
// ---------------------------------------------------------------------------

pub(super) fn classify_word_expansions(words: &[Node]) -> ToolDecision {
    words
        .iter()
        .skip(1)
        .map(super::walker::classify_node_ref)
        .fold(ToolDecision::Allow, stricter)
}

pub(super) fn classify_word_parts(parts: &[Node]) -> ToolDecision {
    parts
        .iter()
        .map(super::walker::classify_node_ref)
        .fold(ToolDecision::Allow, stricter)
}

// ---------------------------------------------------------------------------
// Pipeline pattern detection
// ---------------------------------------------------------------------------

/// Detect `curl/wget | sh/bash` pattern in a pipeline.
pub(super) fn is_download_to_shell(commands: &[Node]) -> bool {
    let cmd_names: Vec<String> = commands
        .iter()
        .filter_map(|c| {
            if let NodeKind::Command { words, .. } = &c.kind {
                util::word_literal_value(&words[0]).map(|v| v.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect();

    let has_download = cmd_names.iter().any(|n| n == "curl" || n == "wget");
    let has_shell = cmd_names
        .iter()
        .any(|n| lists::SHELL_INTERPRETERS.contains(&n.as_str()));

    has_download && has_shell
}
