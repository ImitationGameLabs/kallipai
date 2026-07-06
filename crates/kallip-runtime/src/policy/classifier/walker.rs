//! Core recursive AST walker for shell command classification.

use kallip_common::policy::ExecDecision;
use rable::{ListOperator, Node, NodeKind};

use super::Safety;
use super::catalog;
use super::{ClassifyCtx, delegate, helpers, util};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse a shell command string and classify its safety.
///
/// Returns `Reject` on parse errors (fail-closed).
pub fn classify_command(ctx: &ClassifyCtx<'_>, command: &str) -> Safety {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Safety::Reject {
            reason: "empty commands are not allowed".into(),
        };
    }

    match rable::parse(trimmed, false) {
        Ok(nodes) if nodes.is_empty() => Safety::Reject {
            reason: "command parsed to an empty AST".into(),
        },
        Ok(nodes) => classify_nodes(ctx, &nodes),
        Err(e) => Safety::Reject {
            reason: format!("failed to parse command: {e}"),
        },
    }
}

pub(super) fn classify_nodes(ctx: &ClassifyCtx<'_>, nodes: &[Node]) -> Safety {
    nodes
        .iter()
        .map(|n| classify_node(ctx, n))
        .fold(Safety::ReadOnly, helpers::stricter)
}

/// Public wrapper so sibling modules (helpers, delegate) can recurse back into the walker.
pub(super) fn classify_node_ref(ctx: &ClassifyCtx<'_>, node: &Node) -> Safety {
    classify_node(ctx, node)
}

fn classify_node(ctx: &ClassifyCtx<'_>, node: &Node) -> Safety {
    match &node.kind {
        // --- Simple command ---
        NodeKind::Command {
            assignments,
            words,
            redirects,
        } => {
            if words.is_empty() && assignments.is_empty() && redirects.is_empty() {
                return Safety::Reject {
                    reason: "empty command".into(),
                };
            }
            classify_simple_command(ctx, words, redirects, assignments)
        }

        // --- Pipeline ---
        // The decision is the OR of the component commands: read-only iff every
        // component is. (Composition cannot synthesize side effects beyond the
        // union of the components'.) `curl|sh`-style downloads are hard-rejected.
        NodeKind::Pipeline { commands, .. } => {
            let sub = commands
                .iter()
                .map(|c| classify_node(ctx, c))
                .fold(Safety::ReadOnly, helpers::stricter);
            let base = if helpers::is_download_to_shell(commands) {
                Safety::Reject {
                    reason: concat!(
                        "downloading and piping to a shell is not allowed; ",
                        "download the file first, review it, then execute it",
                    )
                    .into(),
                }
            } else {
                Safety::ReadOnly
            };
            helpers::stricter(base, sub)
        }

        // --- Compound list ---
        // A trailing `&` backgrounds a process the runtime can neither time out
        // nor observe, so any backgrounded item defers to approval regardless of
        // how benign its command is. Otherwise the decision is the OR of the
        // items' commands — safe to compose because the runtime shell is a
        // stateless one-shot process (a persistent-session mode must re-evaluate).
        NodeKind::List { items } => {
            if items
                .iter()
                .any(|item| item.operator == Some(ListOperator::Background))
            {
                return Safety::NeedsApproval;
            }
            items
                .iter()
                .map(|item| classify_node(ctx, &item.command))
                .fold(Safety::ReadOnly, helpers::stricter)
        }

        // --- Control flow ---
        NodeKind::If {
            condition,
            then_body,
            else_body,
            redirects,
        } => helpers::classify_multi(
            ctx,
            &[condition, then_body],
            else_body.as_deref(),
            redirects,
        ),

        NodeKind::While {
            condition,
            body,
            redirects,
        }
        | NodeKind::Until {
            condition,
            body,
            redirects,
        } => helpers::classify_multi(ctx, &[condition, body], None, redirects),

        // For/Select: recurse into both the iteration words and the loop body.
        NodeKind::For {
            words,
            body,
            redirects,
            ..
        }
        | NodeKind::Select {
            words,
            body,
            redirects,
            ..
        } => {
            let mut dec = helpers::classify_multi(ctx, &[body], None, redirects);
            if let Some(ws) = words {
                dec = helpers::stricter(dec, classify_nodes(ctx, ws));
            }
            dec
        }

        NodeKind::ForArith {
            body, redirects, ..
        } => helpers::classify_multi(ctx, &[body], None, redirects),

        // Case: check the match word, each pattern's expressions, and each pattern's body.
        NodeKind::Case {
            word,
            patterns,
            redirects,
        } => {
            let mut dec = classify_node(ctx, word);
            for pat in patterns {
                for pat_node in &pat.patterns {
                    dec = helpers::stricter(dec, classify_node(ctx, pat_node));
                }
                if let Some(body) = &pat.body {
                    dec = helpers::stricter(dec, classify_node(ctx, body));
                }
            }
            helpers::stricter(dec, helpers::classify_redirects(ctx, redirects))
        }

        // --- Function ---
        NodeKind::Function { body, .. } => {
            helpers::stricter(Safety::NeedsApproval, classify_node(ctx, body))
        }

        // --- Grouping ---
        NodeKind::Subshell { body, redirects } | NodeKind::BraceGroup { body, redirects } => {
            helpers::stricter(
                classify_node(ctx, body),
                helpers::classify_redirects(ctx, redirects),
            )
        }

        // --- Redirects ---
        NodeKind::Redirect { .. } => helpers::classify_redirect_node(ctx, node),
        NodeKind::HereDoc { .. } => Safety::NeedsApproval,

        // --- Substitutions ---
        NodeKind::CommandSubstitution { command, .. } => classify_node(ctx, command),
        NodeKind::ProcessSubstitution { command, .. } => {
            helpers::stricter(Safety::NeedsApproval, classify_node(ctx, command))
        }

        // --- Words ---
        NodeKind::Word { parts, .. } => {
            if parts.is_empty() {
                Safety::ReadOnly
            } else {
                helpers::classify_word_parts(ctx, parts)
            }
        }
        NodeKind::WordLiteral { .. } => Safety::ReadOnly,

        // --- Expansions (side-effect-free) ---
        NodeKind::ParamExpansion { .. } | NodeKind::ParamLength { .. } => Safety::ReadOnly,

        NodeKind::ArithmeticExpansion { expression } => expression
            .as_deref()
            .map_or(Safety::ReadOnly, |e| classify_node(ctx, e)),

        NodeKind::ParamIndirect { .. } => Safety::NeedsApproval,

        // --- Arithmetic command ---
        NodeKind::ArithmeticCommand {
            expression,
            redirects,
            ..
        } => {
            let expr_dec = expression
                .as_deref()
                .map_or(Safety::ReadOnly, |e| classify_node(ctx, e));
            helpers::stricter(expr_dec, helpers::classify_redirects(ctx, redirects))
        }

        // --- Negation / Time ---
        NodeKind::Negation { pipeline } | NodeKind::Time { pipeline, .. } => {
            classify_node(ctx, pipeline)
        }

        // --- Coproc ---
        NodeKind::Coproc { command, .. } => {
            helpers::stricter(Safety::NeedsApproval, classify_node(ctx, command))
        }

        // --- Conditional expression ---
        NodeKind::ConditionalExpr { body, redirects } => helpers::stricter(
            classify_node(ctx, body),
            helpers::classify_redirects(ctx, redirects),
        ),
        NodeKind::UnaryTest { operand, .. } => classify_node(ctx, operand),
        NodeKind::BinaryTest { left, right, .. } => {
            helpers::stricter(classify_node(ctx, left), classify_node(ctx, right))
        }
        NodeKind::CondAnd { left, right } | NodeKind::CondOr { left, right } => {
            helpers::stricter(classify_node(ctx, left), classify_node(ctx, right))
        }
        NodeKind::CondNot { operand } => classify_node(ctx, operand),
        NodeKind::CondParen { inner } => classify_node(ctx, inner),
        NodeKind::CondTerm { .. } => Safety::ReadOnly,

        // --- Literals / structural ---
        NodeKind::AnsiCQuote { .. }
        | NodeKind::LocaleString { .. }
        | NodeKind::BraceExpansion { .. }
        | NodeKind::Array { .. }
        | NodeKind::Comment { .. } => Safety::ReadOnly,

        NodeKind::Empty => Safety::Reject {
            reason: "empty command".into(),
        },

        // --- Arithmetic expression nodes (leaf) ---
        NodeKind::ArithNumber { .. }
        | NodeKind::ArithVar { .. }
        | NodeKind::ArithEmpty
        | NodeKind::ArithEscape { .. }
        | NodeKind::ArithDeprecated { .. } => Safety::ReadOnly,

        // --- Arithmetic expression nodes (with children) ---
        NodeKind::ArithBinaryOp { left, right, .. } => {
            helpers::stricter(classify_node(ctx, left), classify_node(ctx, right))
        }
        NodeKind::ArithUnaryOp { operand, .. } => classify_node(ctx, operand),
        NodeKind::ArithPreIncr { operand }
        | NodeKind::ArithPostIncr { operand }
        | NodeKind::ArithPreDecr { operand }
        | NodeKind::ArithPostDecr { operand } => classify_node(ctx, operand),
        NodeKind::ArithAssign { target, value, .. } => {
            helpers::stricter(classify_node(ctx, target), classify_node(ctx, value))
        }
        NodeKind::ArithTernary {
            condition,
            if_true,
            if_false,
        } => {
            let mut dec = classify_node(ctx, condition);
            if let Some(t) = if_true.as_deref() {
                dec = helpers::stricter(dec, classify_node(ctx, t));
            }
            if let Some(f) = if_false.as_deref() {
                dec = helpers::stricter(dec, classify_node(ctx, f));
            }
            dec
        }
        NodeKind::ArithComma { left, right } => {
            helpers::stricter(classify_node(ctx, left), classify_node(ctx, right))
        }
        NodeKind::ArithSubscript { index, .. } => classify_node(ctx, index),
        NodeKind::ArithConcat { parts } => classify_nodes(ctx, parts),
    }
}

// ---------------------------------------------------------------------------
// Command classification
// ---------------------------------------------------------------------------

fn classify_simple_command(
    ctx: &ClassifyCtx<'_>,
    words: &[Node],
    redirects: &[Node],
    assignments: &[Node],
) -> Safety {
    if words.is_empty() {
        return helpers::classify_assignments(ctx, assignments);
    }

    let cmd_name = match util::extract_command_name(&words[0]) {
        Some(name) => name.to_ascii_lowercase(),
        None => {
            return helpers::stricter(Safety::NeedsApproval, classify_node(ctx, &words[0]));
        }
    };

    // 1. Shell interpreter delegation (re-parses inner commands).
    if let Some(safety) = delegate::classify_interpreter_delegate(ctx, &cmd_name, words) {
        return safety;
    }

    // 2. Catalog verdict + exec-policy override.
    //    Allow widens ONLY absent commands (allow-list e.g. `cargo`); for listed
    //    commands the catalog verdict (constraints included) is authoritative, so
    //    `find -delete` / `git push` / `env -S` stay gated. Ask/Deny only narrow.
    let base_dec = apply_override(
        &cmd_name,
        catalog::classify_named_command(ctx.catalog(), &cmd_name, words),
        ctx,
    );

    // 3. Structural child components (redirects, expansions, assignments) fold in
    //    via `stricter`, so they apply regardless of the override (e.g.
    //    `sudo > file` is still caught by the redirect rule).
    let redirect_dec = helpers::classify_redirects(ctx, redirects);
    let expansion_dec = helpers::classify_word_expansions(ctx, words);
    let assignment_dec = helpers::classify_assignments(ctx, assignments);

    [base_dec, redirect_dec, expansion_dec, assignment_dec]
        .into_iter()
        .fold(Safety::ReadOnly, helpers::stricter)
}

/// Fold an exec-policy override onto a catalog verdict.
///
/// `catalog_verdict` is `None` when the command is absent from the catalog.
fn apply_override(
    cmd_name: &str,
    catalog_verdict: Option<Safety>,
    ctx: &ClassifyCtx<'_>,
) -> Safety {
    let deny = || Safety::Reject {
        reason: format!("denied by exec_policy override for '{cmd_name}'"),
    };
    match catalog_verdict {
        None => match ctx.override_for(cmd_name) {
            Some(ExecDecision::Allow) => Safety::ReadOnly,
            Some(ExecDecision::Ask) => Safety::NeedsApproval,
            Some(ExecDecision::Deny) => deny(),
            None => Safety::NeedsApproval,
        },
        Some(catalog_dec) => match ctx.override_for(cmd_name) {
            Some(ExecDecision::Allow) => catalog_dec,
            Some(ExecDecision::Ask) => Safety::NeedsApproval,
            Some(ExecDecision::Deny) => deny(),
            None => catalog_dec,
        },
    }
}
