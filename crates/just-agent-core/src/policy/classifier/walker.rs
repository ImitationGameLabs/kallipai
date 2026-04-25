//! Core recursive AST walker for shell command classification.

use rable::{Node, NodeKind};

use super::super::ToolDecision;
use super::{delegate, helpers, lists, util};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse a shell command string and classify its safety.
///
/// Returns `Deny` on parse errors (fail-closed).
pub fn classify_command(command: &str) -> ToolDecision {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return ToolDecision::Deny { reason: "empty commands are not allowed".into() };
    }

    match rable::parse(trimmed, false) {
        Ok(nodes) if nodes.is_empty() => {
            ToolDecision::Deny { reason: "command parsed to an empty AST".into() }
        }
        Ok(nodes) => classify_nodes(&nodes),
        Err(e) => ToolDecision::Deny { reason: format!("failed to parse command: {e}") },
    }
}

pub(super) fn classify_nodes(nodes: &[Node]) -> ToolDecision {
    nodes
        .iter()
        .map(classify_node)
        .fold(ToolDecision::Allow, helpers::stricter)
}

/// Public wrapper so sibling modules (helpers, delegate) can recurse back into the walker.
pub(super) fn classify_node_ref(node: &Node) -> ToolDecision {
    classify_node(node)
}

fn classify_node(node: &Node) -> ToolDecision {
    match &node.kind {
        // --- Simple command ---
        NodeKind::Command { assignments, words, redirects } => {
            if words.is_empty() && assignments.is_empty() && redirects.is_empty() {
                return ToolDecision::Deny { reason: "empty command".into() };
            }
            classify_simple_command(words, redirects, assignments)
        }

        // --- Pipeline ---
        NodeKind::Pipeline { commands, .. } => {
            if helpers::is_download_to_shell(commands) {
                return ToolDecision::Ask {
                    reason: "downloading and executing remote scripts".into(),
                    dangerous: true,
                };
            }
            let sub = commands
                .iter()
                .map(classify_node)
                .fold(ToolDecision::Allow, helpers::stricter);
            helpers::stricter(
                ToolDecision::Ask {
                    reason: "piped shell commands require approval".into(),
                    dangerous: false,
                },
                sub,
            )
        }

        // --- Compound list ---
        NodeKind::List { items } => {
            let sub = items
                .iter()
                .map(|item| classify_node(&item.command))
                .fold(ToolDecision::Allow, helpers::stricter);
            helpers::stricter(
                ToolDecision::Ask {
                    reason: "compound shell commands require approval".into(),
                    dangerous: false,
                },
                sub,
            )
        }

        // --- Control flow ---
        NodeKind::If { condition, then_body, else_body, redirects } => {
            helpers::classify_multi(&[condition, then_body], else_body.as_deref(), redirects)
        }

        NodeKind::While { condition, body, redirects }
        | NodeKind::Until { condition, body, redirects } => {
            helpers::classify_multi(&[condition, body], None, redirects)
        }

        // For/Select: recurse into both the iteration words and the loop body.
        NodeKind::For { words, body, redirects, .. }
        | NodeKind::Select { words, body, redirects, .. } => {
            let mut dec = helpers::classify_multi(&[body], None, redirects);
            if let Some(ws) = words {
                dec = helpers::stricter(dec, classify_nodes(ws));
            }
            dec
        }

        NodeKind::ForArith { body, redirects, .. } => {
            helpers::classify_multi(&[body], None, redirects)
        }

        // Case: check the match word, each pattern's expressions, and each pattern's body.
        NodeKind::Case { word, patterns, redirects } => {
            let mut dec = classify_node(word);
            for pat in patterns {
                for pat_node in &pat.patterns {
                    dec = helpers::stricter(dec, classify_node(pat_node));
                }
                if let Some(body) = &pat.body {
                    dec = helpers::stricter(dec, classify_node(body));
                }
            }
            helpers::stricter(dec, helpers::classify_redirects(redirects))
        }

        // --- Function ---
        NodeKind::Function { body, .. } => helpers::stricter(
            ToolDecision::Ask {
                reason: "function definition requires approval".into(),
                dangerous: false,
            },
            classify_node(body),
        ),

        // --- Grouping ---
        NodeKind::Subshell { body, redirects } | NodeKind::BraceGroup { body, redirects } => {
            helpers::stricter(classify_node(body), helpers::classify_redirects(redirects))
        }

        // --- Redirects ---
        NodeKind::Redirect { .. } => helpers::classify_redirect_node(node),
        NodeKind::HereDoc { .. } => {
            ToolDecision::Ask { reason: "heredocs require approval".into(), dangerous: false }
        }

        // --- Substitutions ---
        NodeKind::CommandSubstitution { command, .. } => classify_node(command),
        NodeKind::ProcessSubstitution { command, .. } => helpers::stricter(
            ToolDecision::Ask {
                reason: "process substitution requires approval".into(),
                dangerous: false,
            },
            classify_node(command),
        ),

        // --- Words ---
        NodeKind::Word { parts, .. } => {
            if parts.is_empty() {
                ToolDecision::Allow
            } else {
                helpers::classify_word_parts(parts)
            }
        }
        NodeKind::WordLiteral { .. } => ToolDecision::Allow,

        // --- Expansions (side-effect-free) ---
        NodeKind::ParamExpansion { .. } | NodeKind::ParamLength { .. } => ToolDecision::Allow,

        NodeKind::ArithmeticExpansion { expression } => expression
            .as_deref()
            .map_or(ToolDecision::Allow, classify_node),

        NodeKind::ParamIndirect { .. } => ToolDecision::Ask {
            reason: "indirect parameter expansion cannot be statically resolved".into(),
            dangerous: false,
        },

        // --- Arithmetic command ---
        NodeKind::ArithmeticCommand { expression, redirects, .. } => {
            let expr_dec = expression
                .as_deref()
                .map_or(ToolDecision::Allow, classify_node);
            helpers::stricter(expr_dec, helpers::classify_redirects(redirects))
        }

        // --- Negation / Time ---
        NodeKind::Negation { pipeline } | NodeKind::Time { pipeline, .. } => {
            classify_node(pipeline)
        }

        // --- Coproc ---
        NodeKind::Coproc { command, .. } => helpers::stricter(
            ToolDecision::Ask { reason: "coproc requires approval".into(), dangerous: false },
            classify_node(command),
        ),

        // --- Conditional expression ---
        NodeKind::ConditionalExpr { body, redirects } => {
            helpers::stricter(classify_node(body), helpers::classify_redirects(redirects))
        }
        NodeKind::UnaryTest { operand, .. } => classify_node(operand),
        NodeKind::BinaryTest { left, right, .. } => {
            helpers::stricter(classify_node(left), classify_node(right))
        }
        NodeKind::CondAnd { left, right } | NodeKind::CondOr { left, right } => {
            helpers::stricter(classify_node(left), classify_node(right))
        }
        NodeKind::CondNot { operand } => classify_node(operand),
        NodeKind::CondParen { inner } => classify_node(inner),
        NodeKind::CondTerm { .. } => ToolDecision::Allow,

        // --- Literals / structural ---
        NodeKind::AnsiCQuote { .. }
        | NodeKind::LocaleString { .. }
        | NodeKind::BraceExpansion { .. }
        | NodeKind::Array { .. }
        | NodeKind::Comment { .. } => ToolDecision::Allow,

        NodeKind::Empty => ToolDecision::Deny { reason: "empty command".into() },

        // --- Arithmetic expression nodes (leaf) ---
        NodeKind::ArithNumber { .. }
        | NodeKind::ArithVar { .. }
        | NodeKind::ArithEmpty
        | NodeKind::ArithEscape { .. }
        | NodeKind::ArithDeprecated { .. } => ToolDecision::Allow,

        // --- Arithmetic expression nodes (with children) ---
        NodeKind::ArithBinaryOp { left, right, .. } => {
            helpers::stricter(classify_node(left), classify_node(right))
        }
        NodeKind::ArithUnaryOp { operand, .. } => classify_node(operand),
        NodeKind::ArithPreIncr { operand }
        | NodeKind::ArithPostIncr { operand }
        | NodeKind::ArithPreDecr { operand }
        | NodeKind::ArithPostDecr { operand } => classify_node(operand),
        NodeKind::ArithAssign { target, value, .. } => {
            helpers::stricter(classify_node(target), classify_node(value))
        }
        NodeKind::ArithTernary { condition, if_true, if_false } => {
            let mut dec = classify_node(condition);
            if let Some(t) = if_true.as_deref() {
                dec = helpers::stricter(dec, classify_node(t));
            }
            if let Some(f) = if_false.as_deref() {
                dec = helpers::stricter(dec, classify_node(f));
            }
            dec
        }
        NodeKind::ArithComma { left, right } => {
            helpers::stricter(classify_node(left), classify_node(right))
        }
        NodeKind::ArithSubscript { index, .. } => classify_node(index),
        NodeKind::ArithConcat { parts } => classify_nodes(parts),
    }
}

// ---------------------------------------------------------------------------
// Command classification
// ---------------------------------------------------------------------------

fn classify_simple_command(
    words: &[Node],
    redirects: &[Node],
    assignments: &[Node],
) -> ToolDecision {
    if words.is_empty() {
        return helpers::classify_assignments(assignments);
    }

    let cmd_name = match util::extract_command_name(&words[0]) {
        Some(name) => name.to_ascii_lowercase(),
        None => {
            let word_dec = classify_node(&words[0]);
            return helpers::stricter(
                ToolDecision::Ask {
                    reason: "command name contains expansions and cannot be statically classified"
                        .into(),
                    dangerous: false,
                },
                word_dec,
            );
        }
    };

    // 1. Shell interpreter delegation
    if let Some(decision) = delegate::classify_interpreter_delegate(&cmd_name, words) {
        return decision;
    }

    // 2. Dangerous command list
    if lists::DANGEROUS_COMMANDS.contains(&cmd_name.as_str()) {
        return ToolDecision::Ask {
            reason: format!("'{cmd_name}' is a dangerous command"),
            dangerous: true,
        };
    }

    // 3. Argument-aware dangerous check
    if let Some(decision) = lists::check_dangerous_invocation(&cmd_name, words) {
        return decision;
    }

    // 4. Classify child components
    let redirect_dec = helpers::classify_redirects(redirects);
    let expansion_dec = helpers::classify_word_expansions(words);
    let assignment_dec = helpers::classify_assignments(assignments);
    let base_dec = lists::check_allow_list(&cmd_name, words);

    [redirect_dec, expansion_dec, assignment_dec, base_dec]
        .into_iter()
        .fold(ToolDecision::Allow, helpers::stricter)
}
