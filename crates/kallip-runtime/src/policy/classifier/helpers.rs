//! Helper functions for the AST walker — decision merging, redirect/assignment
//! classification, and pipeline pattern detection.

use rable::{Node, NodeKind};

use super::ClassifyCtx;
use super::Safety;
use super::catalog;
use super::util;

/// Return the stricter of two decisions: Reject > NeedsApproval > ReadOnly.
///
/// Reasons are preserved losslessly: a `Reject` reason always wins; two
/// `NeedsApproval` reasons are merged (`"; "`-joined when distinct) so the agent
/// sees every check that tripped rather than just one.
pub(super) fn stricter(a: Safety, b: Safety) -> Safety {
    match (a, b) {
        (Safety::Reject { reason }, _) | (_, Safety::Reject { reason }) => {
            Safety::Reject { reason }
        }
        (Safety::NeedsApproval { reason: ra }, Safety::NeedsApproval { reason: rb }) => {
            let reason = if ra == rb { ra } else { format!("{ra}; {rb}") };
            Safety::NeedsApproval { reason }
        }
        (Safety::NeedsApproval { reason }, Safety::ReadOnly)
        | (Safety::ReadOnly, Safety::NeedsApproval { reason }) => Safety::NeedsApproval { reason },
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

/// Redirect targets whose writes are pure sinks — no observable side effect, so
/// a write redirect to one is treated as read-only. `/dev/null` is the only
/// character device that truly discards: `/dev/full` errors on write, and
/// `/dev/stdout`/`/dev/tty` have observable output. Match is on the literal,
/// quote-stripped target (no canonicalization — the classifier is pre-exec and
/// symlink/TOCTOU-unsafe; the landlock sandbox canonicalizes at exec time).
const READ_ONLY_REDIRECT_SINKS: &[&str] = &["/dev/null"];

/// Whether `s` is an fd number (e.g. `"1"`, `"2"`), the target shape produced by
/// rable for fd-duplication redirects like `2>&1`.
fn is_fd_number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

pub(super) fn classify_redirect_node(ctx: &ClassifyCtx<'_>, node: &Node) -> Safety {
    match &node.kind {
        NodeKind::Redirect { op, target, .. } => {
            let target_lit = util::redirect_target_literal(target);
            let op_dec = classify_redirect_op(op, target_lit);
            // Still recurse into the target so `>$(cmd)` is caught.
            stricter(op_dec, super::walker::classify_node_ref(ctx, target))
        }
        NodeKind::HereDoc { .. } => Safety::NeedsApproval {
            reason: "here-document is not auto-approved".into(),
        },
        _ => super::walker::classify_node_ref(ctx, node),
    }
}

/// Classify a redirect from its `op` and the literal target text.
///
/// Order-sensitive match ladder (first arm wins). rable normalizes both
/// close-fd spellings (`>&-` and `<&-`) and `2>&-` to `op = ">&-"`; it also
/// strips the trailing `-` from a move-fd (`2>&1-`), leaving a digit target
/// under `op = ">&"`.
fn classify_redirect_op(op: &str, target_lit: Option<&str>) -> Safety {
    let is_write_op = matches!(op, ">" | ">>" | ">|" | "<>" | "&>" | "&>>");
    let is_dup_op = matches!(op, ">&" | "<&");
    match (op, target_lit) {
        // fd close: no file opened.
        (">&-", _) => Safety::ReadOnly,
        // fd duplication / move: no file opened.
        (_, Some(t)) if is_dup_op && is_fd_number(t) => Safety::ReadOnly,
        // input redirect: `<` (file) and `<<<` (here-string). Both feed the
        // command's stdin with no observable output side effect on the host
        // (bash may back the here-string with a temp file, same as it does for
        // `<`, but nothing the agent or filesystem observes persists).
        ("<" | "<<<", _) => Safety::ReadOnly,
        // write to a pure sink.
        (_, Some(t)) if is_write_op && READ_ONLY_REDIRECT_SINKS.contains(&t) => Safety::ReadOnly,
        // write-family to a real (or non-literal) target.
        (_, Some(t)) if is_write_op => Safety::NeedsApproval {
            reason: format!("output redirect '{op}' to '{t}'"),
        },
        (_, None) if is_write_op => Safety::NeedsApproval {
            reason: format!("output redirect '{op}' to a non-literal target"),
        },
        // `>&file` / `<&file` bashism: a real target despite the dup-style op.
        // Direction is implied by the op (`>&` writes, `<&` is input-direction),
        // so the reason names the op verbatim rather than assuming "output".
        (_, Some(t)) if is_dup_op => Safety::NeedsApproval {
            reason: format!("redirect '{op}' to '{t}'"),
        },
        // Unknown operator: fail-closed.
        (_, _) => Safety::NeedsApproval {
            reason: format!("redirect operator '{op}' is not auto-approved"),
        },
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
        return Safety::NeedsApproval {
            reason: format!("assignment to sensitive env var '{name}'"),
        };
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
