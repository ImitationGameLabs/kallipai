//! Helper functions for the AST walker — decision merging, redirect/assignment
//! classification, and pipeline pattern detection.

use rable::{Node, NodeKind};

use super::ClassifyCtx;
use super::Safety;
use super::catalog;
use super::util;

/// Return the stricter of two decisions: Reject > NeedsApproval > ReadOnly.
pub(super) fn stricter(a: Safety, b: Safety) -> Safety {
    match (a, b) {
        (Safety::Reject { reason }, _) | (_, Safety::Reject { reason }) => {
            Safety::Reject { reason }
        }
        (Safety::NeedsApproval, _) | (_, Safety::NeedsApproval) => Safety::NeedsApproval,
        (Safety::ReadOnly, Safety::ReadOnly) => Safety::ReadOnly,
    }
}

/// Merge decisions from required nodes, an optional node, and redirects.
pub(super) fn classify_multi(
    ctx: &ClassifyCtx<'_>,
    required: &[&Node],
    optional: Option<&Node>,
    redirects: &[Node],
) -> Safety {
    let mut dec = required
        .iter()
        .map(|n| super::walker::classify_node_ref(ctx, n))
        .fold(Safety::ReadOnly, stricter);
    if let Some(opt) = optional {
        dec = stricter(dec, super::walker::classify_node_ref(ctx, opt));
    }
    stricter(dec, classify_redirects(ctx, redirects))
}

// ---------------------------------------------------------------------------
// Redirect classification
// ---------------------------------------------------------------------------

pub(super) fn classify_redirects(ctx: &ClassifyCtx<'_>, redirects: &[Node]) -> Safety {
    redirects
        .iter()
        .map(|n| classify_redirect_node(ctx, n))
        .fold(Safety::ReadOnly, stricter)
}

pub(super) fn classify_redirect_node(ctx: &ClassifyCtx<'_>, node: &Node) -> Safety {
    match &node.kind {
        NodeKind::Redirect { op, target, .. } => {
            let op_dec = match op.as_str() {
                "<" => Safety::ReadOnly,
                ">" | ">>" | ">|" | "<>" | "&>" | "&>>" => Safety::NeedsApproval,
                _ => Safety::NeedsApproval,
            };
            stricter(op_dec, super::walker::classify_node_ref(ctx, target))
        }
        NodeKind::HereDoc { .. } => Safety::NeedsApproval,
        _ => super::walker::classify_node_ref(ctx, node),
    }
}

// ---------------------------------------------------------------------------
// Assignment classification
// ---------------------------------------------------------------------------

/// Classify variable assignments preceding a command.
///
/// Checks both the variable name (sensitive env vars like PATH) and the
/// value parts (which may contain command substitutions).
pub(super) fn classify_assignments(ctx: &ClassifyCtx<'_>, assignments: &[Node]) -> Safety {
    assignments
        .iter()
        .map(|a| {
            let name_dec = classify_assignment_name(a);
            let parts_dec = match &a.kind {
                NodeKind::Word { parts, .. } if !parts.is_empty() => {
                    classify_word_parts(ctx, parts)
                }
                _ => Safety::ReadOnly,
            };
            stricter(name_dec, parts_dec)
        })
        .fold(Safety::ReadOnly, stricter)
}

fn classify_assignment_name(assignment: &Node) -> Safety {
    let name = match &assignment.kind {
        NodeKind::Word { value, .. } => value.split('=').next().unwrap_or(""),
        _ => return Safety::ReadOnly,
    };
    if catalog::SENSITIVE_ENV_VARS
        .iter()
        .any(|v| v.eq_ignore_ascii_case(name))
    {
        return Safety::NeedsApproval;
    }
    Safety::ReadOnly
}

// ---------------------------------------------------------------------------
// Word expansion classification
// ---------------------------------------------------------------------------

pub(super) fn classify_word_expansions(ctx: &ClassifyCtx<'_>, words: &[Node]) -> Safety {
    words
        .iter()
        .skip(1)
        .map(|n| super::walker::classify_node_ref(ctx, n))
        .fold(Safety::ReadOnly, stricter)
}

pub(super) fn classify_word_parts(ctx: &ClassifyCtx<'_>, parts: &[Node]) -> Safety {
    parts
        .iter()
        .map(|n| super::walker::classify_node_ref(ctx, n))
        .fold(Safety::ReadOnly, stricter)
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
                words
                    .first()
                    .and_then(|w| util::word_literal_value(w))
                    .map(|v| v.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect();

    let has_download = cmd_names.iter().any(|n| n == "curl" || n == "wget");
    let has_shell = cmd_names
        .iter()
        .any(|n| catalog::SHELL_INTERPRETERS.contains(&n.as_str()));

    has_download && has_shell
}
