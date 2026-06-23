//! Core recursive AST walker for shell command classification.

use rable::{ListOperator, Node, NodeKind};

use super::Safety;
use super::catalog::{self, CommandSpec};
use super::{delegate, helpers, util};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse a shell command string and classify its safety against `catalog`.
///
/// Returns `Reject` on parse errors (fail-closed).
pub fn classify_command(catalog: &'static [CommandSpec], command: &str) -> Safety {
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
        Ok(nodes) => classify_nodes(catalog, &nodes),
        Err(e) => Safety::Reject {
            reason: format!("failed to parse command: {e}"),
        },
    }
}

pub(super) fn classify_nodes(catalog: &'static [CommandSpec], nodes: &[Node]) -> Safety {
    nodes
        .iter()
        .map(|n| classify_node(catalog, n))
        .fold(Safety::ReadOnly, helpers::stricter)
}

/// Public wrapper so sibling modules (helpers, delegate) can recurse back into the walker.
pub(super) fn classify_node_ref(catalog: &'static [CommandSpec], node: &Node) -> Safety {
    classify_node(catalog, node)
}

fn classify_node(catalog: &'static [CommandSpec], node: &Node) -> Safety {
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
            classify_simple_command(catalog, words, redirects, assignments)
        }

        // --- Pipeline ---
        // The decision is the OR of the component commands: read-only iff every
        // component is. (Composition cannot synthesize side effects beyond the
        // union of the components'.) `curl|sh`-style downloads are hard-rejected.
        NodeKind::Pipeline { commands, .. } => {
            let sub = commands
                .iter()
                .map(|c| classify_node(catalog, c))
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
                .map(|item| classify_node(catalog, &item.command))
                .fold(Safety::ReadOnly, helpers::stricter)
        }

        // --- Control flow ---
        NodeKind::If {
            condition,
            then_body,
            else_body,
            redirects,
        } => helpers::classify_multi(
            catalog,
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
        } => helpers::classify_multi(catalog, &[condition, body], None, redirects),

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
            let mut dec = helpers::classify_multi(catalog, &[body], None, redirects);
            if let Some(ws) = words {
                dec = helpers::stricter(dec, classify_nodes(catalog, ws));
            }
            dec
        }

        NodeKind::ForArith {
            body, redirects, ..
        } => helpers::classify_multi(catalog, &[body], None, redirects),

        // Case: check the match word, each pattern's expressions, and each pattern's body.
        NodeKind::Case {
            word,
            patterns,
            redirects,
        } => {
            let mut dec = classify_node(catalog, word);
            for pat in patterns {
                for pat_node in &pat.patterns {
                    dec = helpers::stricter(dec, classify_node(catalog, pat_node));
                }
                if let Some(body) = &pat.body {
                    dec = helpers::stricter(dec, classify_node(catalog, body));
                }
            }
            helpers::stricter(dec, helpers::classify_redirects(catalog, redirects))
        }

        // --- Function ---
        NodeKind::Function { body, .. } => {
            helpers::stricter(Safety::NeedsApproval, classify_node(catalog, body))
        }

        // --- Grouping ---
        NodeKind::Subshell { body, redirects } | NodeKind::BraceGroup { body, redirects } => {
            helpers::stricter(
                classify_node(catalog, body),
                helpers::classify_redirects(catalog, redirects),
            )
        }

        // --- Redirects ---
        NodeKind::Redirect { .. } => helpers::classify_redirect_node(catalog, node),
        NodeKind::HereDoc { .. } => Safety::NeedsApproval,

        // --- Substitutions ---
        NodeKind::CommandSubstitution { command, .. } => classify_node(catalog, command),
        NodeKind::ProcessSubstitution { command, .. } => {
            helpers::stricter(Safety::NeedsApproval, classify_node(catalog, command))
        }

        // --- Words ---
        NodeKind::Word { parts, .. } => {
            if parts.is_empty() {
                Safety::ReadOnly
            } else {
                helpers::classify_word_parts(catalog, parts)
            }
        }
        NodeKind::WordLiteral { .. } => Safety::ReadOnly,

        // --- Expansions (side-effect-free) ---
        NodeKind::ParamExpansion { .. } | NodeKind::ParamLength { .. } => Safety::ReadOnly,

        NodeKind::ArithmeticExpansion { expression } => expression
            .as_deref()
            .map_or(Safety::ReadOnly, |e| classify_node(catalog, e)),

        NodeKind::ParamIndirect { .. } => Safety::NeedsApproval,

        // --- Arithmetic command ---
        NodeKind::ArithmeticCommand {
            expression,
            redirects,
            ..
        } => {
            let expr_dec = expression
                .as_deref()
                .map_or(Safety::ReadOnly, |e| classify_node(catalog, e));
            helpers::stricter(expr_dec, helpers::classify_redirects(catalog, redirects))
        }

        // --- Negation / Time ---
        NodeKind::Negation { pipeline } | NodeKind::Time { pipeline, .. } => {
            classify_node(catalog, pipeline)
        }

        // --- Coproc ---
        NodeKind::Coproc { command, .. } => {
            helpers::stricter(Safety::NeedsApproval, classify_node(catalog, command))
        }

        // --- Conditional expression ---
        NodeKind::ConditionalExpr { body, redirects } => helpers::stricter(
            classify_node(catalog, body),
            helpers::classify_redirects(catalog, redirects),
        ),
        NodeKind::UnaryTest { operand, .. } => classify_node(catalog, operand),
        NodeKind::BinaryTest { left, right, .. } => {
            helpers::stricter(classify_node(catalog, left), classify_node(catalog, right))
        }
        NodeKind::CondAnd { left, right } | NodeKind::CondOr { left, right } => {
            helpers::stricter(classify_node(catalog, left), classify_node(catalog, right))
        }
        NodeKind::CondNot { operand } => classify_node(catalog, operand),
        NodeKind::CondParen { inner } => classify_node(catalog, inner),
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
            helpers::stricter(classify_node(catalog, left), classify_node(catalog, right))
        }
        NodeKind::ArithUnaryOp { operand, .. } => classify_node(catalog, operand),
        NodeKind::ArithPreIncr { operand }
        | NodeKind::ArithPostIncr { operand }
        | NodeKind::ArithPreDecr { operand }
        | NodeKind::ArithPostDecr { operand } => classify_node(catalog, operand),
        NodeKind::ArithAssign { target, value, .. } => helpers::stricter(
            classify_node(catalog, target),
            classify_node(catalog, value),
        ),
        NodeKind::ArithTernary {
            condition,
            if_true,
            if_false,
        } => {
            let mut dec = classify_node(catalog, condition);
            if let Some(t) = if_true.as_deref() {
                dec = helpers::stricter(dec, classify_node(catalog, t));
            }
            if let Some(f) = if_false.as_deref() {
                dec = helpers::stricter(dec, classify_node(catalog, f));
            }
            dec
        }
        NodeKind::ArithComma { left, right } => {
            helpers::stricter(classify_node(catalog, left), classify_node(catalog, right))
        }
        NodeKind::ArithSubscript { index, .. } => classify_node(catalog, index),
        NodeKind::ArithConcat { parts } => classify_nodes(catalog, parts),
    }
}

// ---------------------------------------------------------------------------
// Command classification
// ---------------------------------------------------------------------------

fn classify_simple_command(
    catalog: &'static [CommandSpec],
    words: &[Node],
    redirects: &[Node],
    assignments: &[Node],
) -> Safety {
    if words.is_empty() {
        return helpers::classify_assignments(catalog, assignments);
    }

    let cmd_name = match util::extract_command_name(&words[0]) {
        Some(name) => name.to_ascii_lowercase(),
        None => {
            return helpers::stricter(Safety::NeedsApproval, classify_node(catalog, &words[0]));
        }
    };

    // 1. Shell interpreter delegation (re-parses inner commands).
    if let Some(safety) = delegate::classify_interpreter_delegate(catalog, &cmd_name, words) {
        return safety;
    }

    // 2. Catalog: explicit allow vs not-in-catalog → NeedsApproval, plus per-spec
    //    subcommand/flag/predicate guards.
    let base_dec = catalog::classify_named_command(catalog, &cmd_name, words);

    // 3. Child components (redirects, expansions, assignments).
    let redirect_dec = helpers::classify_redirects(catalog, redirects);
    let expansion_dec = helpers::classify_word_expansions(catalog, words);
    let assignment_dec = helpers::classify_assignments(catalog, assignments);

    [base_dec, redirect_dec, expansion_dec, assignment_dec]
        .into_iter()
        .fold(Safety::ReadOnly, helpers::stricter)
}
