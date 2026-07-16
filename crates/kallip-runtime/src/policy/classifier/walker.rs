//! Core recursive AST walker for shell command classification.

use kallip_common::policy::{ExecDecision, PolicyPreset};
use rable::{ListOperator, Node, NodeKind};

use super::ToolDecision;
use super::catalog;
use super::{ClassifyCtx, delegate, helpers, util};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse a shell command string and classify it under the active preset.
///
/// Fail-closed: an empty, unparseable, or empty-AST command is `Deny` regardless
/// of preset. The `allow-all` short-circuit sits here (see below).
pub fn classify_command(ctx: &ClassifyCtx<'_>, command: &str) -> ToolDecision {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return ToolDecision::Deny {
            reason: "empty commands are not allowed".into(),
        };
    }

    let nodes = match rable::parse(trimmed, false) {
        Ok(nodes) if !nodes.is_empty() => nodes,
        Ok(_) => {
            return ToolDecision::Deny {
                reason: "command parsed to an empty AST".into(),
            };
        }
        Err(e) => {
            return ToolDecision::Deny {
                reason: format!("failed to parse command: {e}"),
            };
        }
    };

    // `allow-all` short-circuit, placed AFTER parsing (fail-closed) and BEFORE the
    // walk. This must live at `classify_command` (the single public entry), NOT at
    // `classify_nodes`: interpreter delegation (`bash -c '...'`) re-enters via
    // `classify_nodes`, and those inner walks must stay under this short-circuit's
    // umbrella (a single preset check for the whole command) rather than
    // re-evaluating the preset per inner node.
    if ctx.preset() == PolicyPreset::AllowAll {
        return ToolDecision::Allow;
    }

    classify_nodes(ctx, &nodes)
}

pub(super) fn classify_nodes(ctx: &ClassifyCtx<'_>, nodes: &[Node]) -> ToolDecision {
    nodes
        .iter()
        .map(|n| classify_node(ctx, n))
        .fold(ToolDecision::Allow, helpers::stricter)
}

/// Public wrapper so sibling modules (helpers, delegate) can recurse back into the walker.
pub(super) fn classify_node_ref(ctx: &ClassifyCtx<'_>, node: &Node) -> ToolDecision {
    classify_node(ctx, node)
}

fn classify_node(ctx: &ClassifyCtx<'_>, node: &Node) -> ToolDecision {
    match &node.kind {
        // --- Simple command ---
        NodeKind::Command {
            assignments,
            words,
            redirects,
        } => {
            if words.is_empty() && assignments.is_empty() && redirects.is_empty() {
                return ToolDecision::Deny {
                    reason: "empty command".into(),
                };
            }
            classify_simple_command(ctx, words, redirects, assignments)
        }

        // --- Pipeline ---
        // The decision is the OR of the component commands: read-only iff every
        // component is. (Composition cannot synthesize side effects beyond the
        // union of the components'.) `curl|sh`-style downloads are hard-denied.
        NodeKind::Pipeline { commands, .. } => {
            let sub = commands
                .iter()
                .map(|c| classify_node(ctx, c))
                .fold(ToolDecision::Allow, helpers::stricter);
            let base = if helpers::is_download_to_shell(commands) {
                ToolDecision::Deny {
                    reason: concat!(
                        "downloading and piping to a shell is not allowed; ",
                        "download the file first, review it, then execute it",
                    )
                    .into(),
                }
            } else {
                ToolDecision::Allow
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
                return helpers::needs_approval(
                    ctx,
                    "background operator '&' runs an unobservable process; \
                        use the runtime's native background execution instead"
                        .into(),
                );
            }
            items
                .iter()
                .map(|item| classify_node(ctx, &item.command))
                .fold(ToolDecision::Allow, helpers::stricter)
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
        NodeKind::Function { body, .. } => helpers::stricter(
            helpers::needs_approval(
                ctx,
                "function definitions can later execute arbitrary code".into(),
            ),
            classify_node(ctx, body),
        ),

        // --- Grouping ---
        NodeKind::Subshell { body, redirects } | NodeKind::BraceGroup { body, redirects } => {
            helpers::stricter(
                classify_node(ctx, body),
                helpers::classify_redirects(ctx, redirects),
            )
        }

        // --- Redirects ---
        NodeKind::Redirect { .. } => helpers::classify_redirect_node(ctx, node),
        NodeKind::HereDoc { .. } => {
            helpers::needs_approval(ctx, "here-document is not auto-approved".into())
        }

        // --- Substitutions ---
        NodeKind::CommandSubstitution { command, .. } => classify_node(ctx, command),
        NodeKind::ProcessSubstitution { command, .. } => helpers::stricter(
            helpers::needs_approval(ctx, "process substitution runs a command".into()),
            classify_node(ctx, command),
        ),

        // --- Words ---
        NodeKind::Word { parts, .. } => {
            if parts.is_empty() {
                ToolDecision::Allow
            } else {
                helpers::classify_word_parts(ctx, parts)
            }
        }
        NodeKind::WordLiteral { .. } => ToolDecision::Allow,

        // --- Expansions (side-effect-free) ---
        NodeKind::ParamExpansion { .. } | NodeKind::ParamLength { .. } => ToolDecision::Allow,

        NodeKind::ArithmeticExpansion { expression } => expression
            .as_deref()
            .map_or(ToolDecision::Allow, |e| classify_node(ctx, e)),

        NodeKind::ParamIndirect { .. } => helpers::needs_approval(
            ctx,
            "indirect parameter expansion can read arbitrary variables".into(),
        ),

        // --- Arithmetic command ---
        NodeKind::ArithmeticCommand {
            expression,
            redirects,
            ..
        } => {
            let expr_dec = expression
                .as_deref()
                .map_or(ToolDecision::Allow, |e| classify_node(ctx, e));
            helpers::stricter(expr_dec, helpers::classify_redirects(ctx, redirects))
        }

        // --- Negation / Time ---
        NodeKind::Negation { pipeline } | NodeKind::Time { pipeline, .. } => {
            classify_node(ctx, pipeline)
        }

        // --- Coproc ---
        NodeKind::Coproc { command, .. } => helpers::stricter(
            helpers::needs_approval(
                ctx,
                "coproc runs an unobservable background process; \
                    use the runtime's native background execution instead"
                    .into(),
            ),
            classify_node(ctx, command),
        ),

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
        NodeKind::CondTerm { .. } => ToolDecision::Allow,

        // --- Literals / structural ---
        NodeKind::AnsiCQuote { .. }
        | NodeKind::LocaleString { .. }
        | NodeKind::BraceExpansion { .. }
        | NodeKind::Array { .. }
        | NodeKind::Comment { .. } => ToolDecision::Allow,

        NodeKind::Empty => ToolDecision::Deny {
            reason: "empty command".into(),
        },

        // --- Arithmetic expression nodes (leaf) ---
        NodeKind::ArithNumber { .. }
        | NodeKind::ArithVar { .. }
        | NodeKind::ArithEmpty
        | NodeKind::ArithEscape { .. }
        | NodeKind::ArithDeprecated { .. } => ToolDecision::Allow,

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
) -> ToolDecision {
    if words.is_empty() {
        return helpers::classify_assignments(ctx, assignments);
    }

    let cmd_name = match util::extract_command_name(&words[0]) {
        Some(name) => name.to_ascii_lowercase(),
        None => {
            return helpers::stricter(
                helpers::needs_approval(ctx, "command name is not a literal word".into()),
                classify_node(ctx, &words[0]),
            );
        }
    };

    // 1. Shell interpreter delegation (re-parses inner commands).
    if let Some(decision) = delegate::classify_interpreter_delegate(ctx, &cmd_name, words) {
        return decision;
    }

    // 2. Catalog verdict + exec-policy override.
    //    Allow widens ONLY absent commands (allow-list e.g. `cargo`); for listed
    //    commands the catalog verdict (constraints included) is authoritative, so
    //    `find -delete` / `git push` / `env -S` stay gated. Ask/Deny only narrow.
    let base_dec = apply_override(
        ctx,
        &cmd_name,
        catalog::classify_named_command(ctx, &cmd_name, words),
    );

    // 3. Structural child components (redirects, expansions, assignments) fold in
    //    via `stricter`, so they apply regardless of the override (e.g.
    //    `sudo > file` is still caught by the redirect rule).
    let redirect_dec = helpers::classify_redirects(ctx, redirects);
    let expansion_dec = helpers::classify_word_expansions(ctx, words);
    let assignment_dec = helpers::classify_assignments(ctx, assignments);

    [base_dec, redirect_dec, expansion_dec, assignment_dec]
        .into_iter()
        .fold(ToolDecision::Allow, helpers::stricter)
}

/// Fold an exec-policy override onto a catalog verdict.
///
/// `catalog_verdict` is `None` when the command is absent from the catalog. The
/// decision lattice:
///
/// - **Builtin denylist** (hard floor, checked first): always `Deny`, cannot be
///   widened by any override. Fires inside delegated bodies too (`bash -c 'sed …'`
///   re-enters here via interpreter delegation).
/// - **Override `Deny`/`Ask`**: authoritative — a deliberate supervisor decision,
///   emitted as `Deny`/`Ask` directly and NOT subject to preset resolution. (Only
///   the soft "command absent from catalog, no override" fallback tracks the
///   preset; an explicit override remains meaningful under `auto`.)
/// - **Override `Allow`**: widens an absent command to `Allow`; for a catalog
///   command the catalog verdict (constraints included) stays authoritative.
/// - **No override, absent from catalog**: the soft `needs_approval` fallback
///   (`Ask` under `default`, `Allow` under `auto`).
///
/// A reason-bearing override is surfaced verbatim; a reason-less override falls
/// back to a generic message. Reason strings are built lazily inside the arms.
fn apply_override(
    ctx: &ClassifyCtx<'_>,
    cmd_name: &str,
    catalog_verdict: Option<ToolDecision>,
) -> ToolDecision {
    // Hard floor: builtin-denied commands are always Deny, regardless of the
    // catalog verdict or any per-agent override. Checked first so it cannot be
    // widened. Fires inside delegated bodies too (`bash -c 'sed …'` re-enters
    // here via interpreter delegation).
    if let Some(reason) = super::builtin_deny_reason(cmd_name) {
        return ToolDecision::Deny {
            reason: reason.to_string(),
        };
    }

    let ov = ctx.override_for(cmd_name);
    let decision = ov.map(|o| o.decision);
    // Borrow the override's reason only when an arm below actually needs it.
    let reason = || ov.and_then(|o| o.reason.as_deref());

    match catalog_verdict {
        None => match decision {
            Some(ExecDecision::Allow) => ToolDecision::Allow,
            Some(ExecDecision::Ask) => ToolDecision::Ask {
                reason: Some(
                    reason()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("'{cmd_name}' is set to 'ask' by exec_policy")),
                ),
            },
            Some(ExecDecision::Deny) => ToolDecision::Deny {
                reason: reason()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("denied by exec_policy override for '{cmd_name}'")),
            },
            None => helpers::needs_approval(
                ctx,
                format!("'{cmd_name}' is not in the read-only catalog"),
            ),
        },
        Some(catalog_dec) => match decision {
            Some(ExecDecision::Allow) => catalog_dec,
            Some(ExecDecision::Ask) => ToolDecision::Ask {
                reason: Some(
                    reason()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("'{cmd_name}' is set to 'ask' by exec_policy")),
                ),
            },
            Some(ExecDecision::Deny) => ToolDecision::Deny {
                reason: reason()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("denied by exec_policy override for '{cmd_name}'")),
            },
            None => catalog_dec,
        },
    }
}
