//! Rule-triggered hook matching for `bash_exec` commands.
//!
//! Hooks observe a command and match operator-declared argv prefixes against
//! every simple-command segment, or a structural condition (a write
//! redirection). They never gate a call (that is the
//! classifier's job) and never touch the tool result; the executor delivers
//! matched notes as independent `[hook] NOTE:` messages on the notice channel
//! (design: `.draft/design/toolcall-hooks.md`).
//!
//! The same `rable` parse and the same literal-word policy as the classifier
//! are reused — there is no second parser, only a different walk over the same
//! AST.

use rable::{Node, NodeKind};

use super::util;

/// When a hook fires, relative to the tool call.
///
/// v1 implements post-call notes only; the operator surface does not carry
/// a phase at all (pre hooks are a v1.1+ concern), so the enum exists for
/// the runtime's internal vocabulary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HookPhase {
    #[default]
    Post,
    Pre,
}

/// One operator-declared hook rule.
///
/// Rules come only from operator config — never from skills, subagents,
/// environment, or file contents read during a session (trust boundary in the
/// design). Rules ship as a builtin preset, and operators adjust them in
/// exec_hooks.toml's `[overrides]` map — the key is the command prefix to
/// observe ("git commit"), the value the note, `off`, or a `{ note }`
/// table; see `config::load_exec_hook_rules` for the format and fail-closed
/// semantics.
/// What a rule matches a call against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Argv prefix tokens, matched against every simple-command segment and
    /// built from an exec_hooks.toml `[overrides]` key (whitespace-split,
    /// lowercased). An empty prefix matches nothing and is dropped at load —
    /// it must not match every call.
    Prefix(Vec<String>),
    /// Any write redirection (`>`, `>>`, `>|`, `<>`, `&>`, `&>>`) whose
    /// literal target is not a pure sink (`/dev/null`) — the shell-edit
    /// shape. Its `[overrides]` key is the reserved pseudo-prefix
    /// [`WRITE_REDIRECT_KEY`]; any other `@`-key is rejected at load, so a
    /// pseudo-prefix can never silently shadow a prefix rule.
    WriteRedirect,
}

/// The `[overrides]` key that addresses [`Trigger::WriteRedirect`].
pub const WRITE_REDIRECT_KEY: &str = "@write-redirect";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookRule {
    /// Tool the rule observes. v1 accepts `bash_exec` only (validation drops
    /// anything else with a warning); the field is kept for later extension.
    pub tool: String,
    pub trigger: Trigger,
    pub phase: HookPhase,
    /// Static note text, self-conditional by convention ("if the skill was not
    /// consulted …") so a redundant note costs one line, not a wrong claim.
    pub note: String,
}

impl HookRule {
    /// The canonical `[overrides]` key this rule is addressed by.
    pub fn override_key(&self) -> String {
        match &self.trigger {
            Trigger::Prefix(tokens) => tokens.join(" "),
            Trigger::WriteRedirect => WRITE_REDIRECT_KEY.to_owned(),
        }
    }
}

/// Literal argv tokens of one simple-command segment. A `None` token is a
/// non-literal word (expansion) — it can never satisfy a prefix token, so
/// `git "$(x)" commit` does not match `["git", "commit"]`.
pub type Segment = Vec<Option<String>>;

/// One pass over the command: the token vectors of every simple-command
/// segment, and whether any write redirection was seen.
///
/// `make && git commit` yields two segments; `echo "git commit"` yields one
/// (the argument is a word, not a segment); `echo "$(git commit)"` yields two
/// (a command substitution really executes). Interpreter delegation is
/// deliberately NOT re-parsed in v1: `bash -c 'git commit'` yields the single
/// outer segment `bash -c …`, because the inner script is a word argument —
/// pinned by test so a v1.1 delegate-style re-parse cannot silently change
/// the semantics. Unparseable or empty input yields no segments (no false
/// notes).
pub fn scan_command(command: &str) -> (Vec<Segment>, bool) {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return (Vec::new(), false);
    }
    let Ok(nodes) = rable::parse(trimmed, false) else {
        return (Vec::new(), false);
    };
    let mut scan = Scan::default();
    collect_segments(&nodes, &mut scan);
    (scan.segments, scan.write_redirect)
}

/// Walk accumulator: command segments plus the write-redirect flag.
#[derive(Default)]
struct Scan {
    segments: Vec<Segment>,
    write_redirect: bool,
}

/// Observe one node list of redirections: a write operator with a non-sink
/// (or non-literal — unverifiable, so conservatively treated as a write)
/// target marks the call. A here-document alone feeds stdin; the write, if
/// any, comes from its sibling redirect.
fn observe_redirects(redirects: &[Node], scan: &mut Scan) {
    for node in redirects {
        if let NodeKind::Redirect { op, target, .. } = &node.kind
            && super::helpers::WRITE_REDIRECT_OPS.contains(&op.as_str())
            && !util::redirect_target_literal(target)
                .is_some_and(|t| super::helpers::READ_ONLY_REDIRECT_SINKS.contains(&t))
        {
            scan.write_redirect = true;
        }
    }
}

/// Matching rules, in canonical-key order (a rule fires at most once per
/// call even if several segments match).
///
/// Prefix match: `prefix.len() <= segment.len()` and every prefix token equals
/// the corresponding segment token, comparing lowercased (segments are
/// lowercased at extraction). No substring matching anywhere; an empty prefix
/// matches nothing. A `WriteRedirect` rule matches iff the scan saw a write
/// redirection.
pub fn hook_matches<'a>(
    rules: &'a [HookRule],
    segments: &[Segment],
    write_redirect: bool,
) -> Vec<&'a HookRule> {
    rules
        .iter()
        .filter(|rule| match &rule.trigger {
            Trigger::Prefix(prefix) => {
                !prefix.is_empty()
                    && segments.iter().any(|segment| {
                        prefix.len() <= segment.len()
                            && prefix.iter().enumerate().all(|(i, token)| {
                                segment[i]
                                    .as_deref()
                                    .is_some_and(|tok| tok == token.to_ascii_lowercase())
                            })
                    })
            }
            Trigger::WriteRedirect => write_redirect,
        })
        .collect()
}

/// The literal, unquoted, lowercased value of a word, or `None` when the word
/// contains expansions (no static value to match against).
fn literal_token(word: &Node) -> Option<String> {
    util::word_literal_value(word)
        .map(util::strip_surrounding_quotes)
        .map(|tok| tok.to_ascii_lowercase())
}

/// Walk the AST mirroring the classifier's shapes, collecting one token vector
/// per `NodeKind::Command` and observing redirects on every shape the
/// classifier gates — commands, compounds, arithmetic and conditional
/// expressions (case pattern words contribute nothing beyond the word-part
/// recursion that reaches their substitutions; a missed note is acceptable;
/// a wrong one is not).
fn collect_segments(nodes: &[Node], scan: &mut Scan) {
    for node in nodes {
        match &node.kind {
            NodeKind::Command {
                words, redirects, ..
            } => {
                if !words.is_empty() {
                    scan.segments
                        .push(words.iter().map(literal_token).collect());
                }
                // A word argument may embed a command/process substitution
                // (`echo "$(git commit)"`) — it executes, so its inner
                // segments are observed too. Literal script arguments (the
                // `'…'` of `bash -c`) have no substitution parts and stay
                // unobserved — the v1 outer-only pin.
                for word in words {
                    if let NodeKind::Word { parts, .. } = &word.kind {
                        collect_segments(parts, scan);
                    }
                }
                observe_redirects(redirects, scan);
            }
            NodeKind::Pipeline { commands, .. } => collect_segments(commands, scan),
            NodeKind::List { items } => {
                for item in items {
                    collect_node(&item.command, scan);
                }
            }
            NodeKind::If {
                condition,
                then_body,
                else_body,
                redirects,
                ..
            } => {
                collect_node(condition, scan);
                collect_node(then_body, scan);
                if let Some(else_body) = else_body {
                    collect_node(else_body, scan);
                }
                observe_redirects(redirects, scan);
            }
            NodeKind::While {
                condition,
                body,
                redirects,
                ..
            }
            | NodeKind::Until {
                condition,
                body,
                redirects,
                ..
            } => {
                collect_node(condition, scan);
                collect_node(body, scan);
                observe_redirects(redirects, scan);
            }
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
                if let Some(words) = words {
                    collect_segments(words, scan);
                }
                collect_node(body, scan);
                observe_redirects(redirects, scan);
            }
            NodeKind::ForArith {
                body, redirects, ..
            } => {
                collect_node(body, scan);
                observe_redirects(redirects, scan);
            }
            NodeKind::Case {
                word,
                patterns,
                redirects,
                ..
            } => {
                collect_node(word, scan);
                for pattern in patterns {
                    if let Some(body) = &pattern.body {
                        collect_node(body, scan);
                    }
                }
                observe_redirects(redirects, scan);
            }
            // The walker gates these two shapes' redirects (and their
            // inner substitutions) — the scan mirrors both.
            NodeKind::ArithmeticCommand {
                expression,
                redirects,
                ..
            } => {
                if let Some(expression) = expression.as_deref() {
                    collect_node(expression, scan);
                }
                observe_redirects(redirects, scan);
            }
            NodeKind::ConditionalExpr { body, redirects } => {
                collect_node(body, scan);
                observe_redirects(redirects, scan);
            }
            NodeKind::Function { body, .. } => collect_node(body, scan),
            // `! git commit` and `time git commit` run the command; a coproc
            // runs it unobservably in the background (the classifier gates
            // it for exactly that reason) — the command still executes, so
            // its segment is observed.
            NodeKind::Negation { pipeline } | NodeKind::Time { pipeline, .. } => {
                collect_node(pipeline, scan)
            }
            NodeKind::Coproc { command, .. } => collect_node(command, scan),
            NodeKind::Subshell {
                body, redirects, ..
            }
            | NodeKind::BraceGroup {
                body, redirects, ..
            } => {
                collect_node(body, scan);
                observe_redirects(redirects, scan);
            }
            NodeKind::CommandSubstitution { command, .. }
            | NodeKind::ProcessSubstitution { command, .. } => collect_node(command, scan),
            // Words can embed command substitutions (`case $(cmd) …`); the
            // substitution arms above catch them on recursion into parts.
            NodeKind::Word { parts, .. } => collect_segments(parts, scan),
            _ => {}
        }
    }
}

fn collect_node(node: &Node, scan: &mut Scan) {
    collect_segments(std::slice::from_ref(node), scan);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(prefix: &[&str], note: &str) -> HookRule {
        HookRule {
            tool: "bash_exec".into(),
            trigger: Trigger::Prefix(prefix.iter().map(|p| (*p).into()).collect()),
            phase: HookPhase::Post,
            note: note.into(),
        }
    }

    fn write_redirect_rule(note: &str) -> HookRule {
        HookRule {
            tool: "bash_exec".into(),
            trigger: Trigger::WriteRedirect,
            phase: HookPhase::Post,
            note: note.into(),
        }
    }

    fn notes(rules: &[HookRule], command: &str) -> Vec<String> {
        let (segments, write_redirect) = scan_command(command);
        hook_matches(rules, &segments, write_redirect)
            .into_iter()
            .map(|r| r.note.clone())
            .collect()
    }

    #[test]
    fn prefix_hit_with_extra_args() {
        let rules = vec![rule(&["git", "commit"], "n")];
        assert_eq!(notes(&rules, "git commit -m 'some message'"), ["n"]);
    }

    #[test]
    fn echo_argument_is_not_a_segment() {
        let rules = vec![rule(&["git", "commit"], "n")];
        // `git commit` inside echo's string argument must not fire.
        assert!(notes(&rules, "echo \"git commit\"").is_empty());
        // Same for a bare single-word echo of it.
        assert!(notes(&rules, "echo git commit").is_empty());
    }

    #[test]
    fn prefix_longer_than_segment_never_matches() {
        let rules = vec![rule(&["git", "commit"], "n")];
        assert!(notes(&rules, "git").is_empty());
        assert!(notes(&rules, "git status").is_empty());
    }

    #[test]
    fn every_segment_of_a_composition_is_checked() {
        let rules = vec![rule(&["git", "commit"], "n")];
        assert_eq!(notes(&rules, "make test && git commit").len(), 1);
        assert_eq!(notes(&rules, "git commit | wc -l").len(), 1);
    }

    /// Negated, timed, and coproc'd commands still run — their segments are
    /// observed like any other.
    #[test]
    fn negation_time_and_coproc_yield_segments() {
        let rules = vec![rule(&["git", "commit"], "n")];
        assert_eq!(notes(&rules, "! git commit").len(), 1);
        assert_eq!(notes(&rules, "time git commit").len(), 1);
        assert_eq!(notes(&rules, "coproc git commit").len(), 1);
    }

    #[test]
    fn matching_is_case_insensitive_both_sides() {
        let rules = vec![rule(&["GiT", "CoMMiT"], "n")];
        assert_eq!(notes(&rules, "GIT commit -m x").len(), 1);
    }

    #[test]
    fn non_literal_word_never_satisfies_a_prefix_token() {
        let rules = vec![rule(&["git", "commit"], "n")];
        // `git "$(x)" commit` collapses positional tokens — must NOT match.
        assert!(notes(&rules, "git \"$(x)\" commit").is_empty());
    }

    /// v1 semantics pin: interpreter delegation is not re-parsed, so the inner
    /// script of `bash -c` is an argument of the outer segment, not a segment.
    /// A v1.1 delegate-style re-parse must change this test deliberately.
    #[test]
    fn bash_c_inner_script_is_not_matched_outer_only() {
        let git_rule = rule(&["git", "commit"], "git");
        let bash_rule = rule(&["bash"], "bash");
        assert!(notes(&[git_rule], "bash -c 'git commit'").is_empty());
        assert_eq!(notes(&[bash_rule], "bash -c 'git commit'").len(), 1);
    }

    #[test]
    fn command_substitution_inside_word_still_yields_segment() {
        let rules = vec![rule(&["git", "commit"], "n")];
        // The substitution really executes — its segment is observed.
        assert_eq!(notes(&rules, "echo \"$(git commit)\"").len(), 1);
    }

    #[test]
    fn quoted_tokens_match_unquoted() {
        let rules = vec![rule(&["git", "commit"], "n")];
        assert_eq!(notes(&rules, "git 'commit' -m x").len(), 1);
    }

    #[test]
    fn two_matching_rules_both_fire_once() {
        let rules = vec![rule(&["git"], "a"), rule(&["git", "commit"], "b")];
        assert_eq!(notes(&rules, "git commit && git status"), ["a", "b"]);
    }

    #[test]
    fn empty_prefix_rule_matches_nothing_even_without_validation() {
        let rules = vec![rule(&[], "n")];
        assert!(notes(&rules, "git commit").is_empty());
    }

    #[test]
    fn unparseable_or_empty_input_yields_no_segments() {
        assert!(scan_command("").0.is_empty());
        assert!(scan_command("   ").0.is_empty());
        // Parse failure (unbalanced) — no segments, no notes.
        assert!(scan_command("if true; then").0.is_empty());
    }

    #[test]
    fn write_redirect_rule_fires_on_write_shapes() {
        let rules = vec![write_redirect_rule("w")];
        // Truncating, appending, combined-output forms, and a bare redirect.
        assert_eq!(notes(&rules, "echo hi > out.txt"), ["w"]);
        assert_eq!(notes(&rules, "cat >> app.log"), ["w"]);
        assert_eq!(notes(&rules, "make &> build.log"), ["w"]);
        assert_eq!(notes(&rules, "> f"), ["w"]);
        // An fd-qualified stderr write is still a write.
        assert_eq!(notes(&rules, "make 2> err.log"), ["w"]);
        // A heredoc feeding a write redirect is the classic edit shape.
        assert_eq!(notes(&rules, "cat > f <<EOF\nline\nEOF"), ["w"]);
        // A non-literal target cannot be checked — conservatively a write.
        assert_eq!(notes(&rules, "echo hi > $(mktemp)").len(), 1);
    }

    #[test]
    fn write_redirect_rule_quiet_on_reads_and_sinks() {
        let rules = vec![write_redirect_rule("w")];
        // Input redirect and heredoc-only feed stdin — no write.
        assert!(notes(&rules, "cat < input.txt").is_empty());
        assert!(notes(&rules, "cat <<EOF\nline\nEOF").is_empty());
        // fd duplication and close open no file.
        assert!(notes(&rules, "make 2>&1").is_empty());
        assert!(notes(&rules, "make 2>&-").is_empty());
        // Pure sink: writing to /dev/null discards.
        assert!(notes(&rules, "echo hi > /dev/null").is_empty());
    }

    /// v1 semantics pin: the inner script of `bash -c` is not observed — the
    /// redirect pin mirrors the prefix pin above it.
    #[test]
    fn write_redirect_rule_outer_only_for_interpreter_scripts() {
        let rules = vec![write_redirect_rule("w")];
        // Inner script redirect unobserved…
        assert!(notes(&rules, "bash -c 'cat > f'").is_empty());
        // …but the outer command's own redirect is.
        assert_eq!(notes(&rules, "bash -c 'x' > f"), ["w"]);
    }

    #[test]
    fn write_redirect_rule_covers_arith_and_conditional_shapes() {
        let rules = vec![write_redirect_rule("w")];
        assert_eq!(notes(&rules, "(( count++ )) > log"), ["w"]);
        assert_eq!(notes(&rules, "[[ -f f ]] > log"), ["w"]);
        // Sink exclusion applies on these shapes as everywhere.
        assert!(notes(&rules, "(( count++ )) > /dev/null").is_empty());
    }

    #[test]
    fn write_redirect_and_prefix_rules_fire_together() {
        let rules = vec![rule(&["git", "commit"], "c"), write_redirect_rule("w")];
        assert_eq!(notes(&rules, "git commit > log"), ["c", "w"])
    }
}
