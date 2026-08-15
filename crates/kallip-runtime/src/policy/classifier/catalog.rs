//! The read-only command catalog — the single source of truth for which shell
//! commands the classifier treats as side-effect-free.
//!
//! A command absent from [`READ_ONLY_CATALOG`] is never auto-approved:
//! [`classify_named_command`] returns `None` for it (the walker then resolves it
//! via the preset). "What is allowed" is therefore an explicit, auditable list
//! rather than an implicit string fallback. Mutating/dangerous commands (`rm`,
//! `sudo`, `dd`, …) are not listed and so defer to approval by default — there is
//! no separate "dangerous list" to keep in sync.

use rable::Node;

use super::ClassifyCtx;
use super::ToolDecision;
use super::helpers::needs_approval;
use super::util;

/// A constraint that can downgrade an otherwise-allowed command to "needs
/// approval" (which the preset then resolves to `Ask` or `Allow`).
#[derive(Clone, Copy, Debug)]
pub(super) enum Constraint {
    /// Only these subcommands are read-only. Any missing, non-literal, or
    /// unlisted subcommand defers to approval. Modeled for commands like `git`,
    /// where some subcommands (`git log`) are read-only and others (`git push`)
    /// are not.
    Subcommands(&'static [&'static str]),
    /// Flags that break read-only-ness even for an otherwise-safe invocation
    /// (e.g. `find -delete`, `sort -o`).
    MutatingFlags(&'static [&'static str]),
    /// A predicate over the command words for cases a flag list cannot express
    /// (e.g. `env <cmd>` running a command operand).
    MutatingPredicate(fn(&[Node]) -> bool),
}

/// One explicitly-allowed command and the constraints that keep it read-only.
#[derive(Clone, Copy, Debug)]
pub(super) struct CommandSpec {
    pub(super) name: &'static str,
    pub(super) constraints: &'static [Constraint],
}

impl CommandSpec {
    /// Apply this spec's constraints to the command words.
    ///
    /// Returns [`ToolDecision::Allow`] only if no constraint trips.
    fn classify(&self, ctx: &ClassifyCtx<'_>, words: &[Node]) -> ToolDecision {
        for constraint in self.constraints {
            match constraint {
                Constraint::Subcommands(safe) => {
                    // words[1] is the subcommand. A bare command (no subcommand)
                    // or a non-literal one is NOT this constraint's concern: a
                    // bare command is a usage error the command itself reports
                    // (e.g. `git` prints help and exits — not a security issue),
                    // and a non-literal subcommand like `git $(cmd)` is caught by
                    // the word-expansion check. Only a literal subcommand outside
                    // the read-only set defers. words.get(1) avoids indexing bare.
                    //
                    // Limitation: a global flag at words[1] (e.g. `git -c x=y log`)
                    // is treated as the subcommand candidate, so such invocations
                    // over-defer. Properly skipping git's value-taking global
                    // flags to locate the real subcommand is a follow-up; the
                    // over-gating direction is safe.
                    let Some(sub) = words.get(1).and_then(util::word_literal_value) else {
                        continue;
                    };
                    if !safe.contains(&sub) {
                        return needs_approval(
                            ctx,
                            format!(
                                "'{} {}' is not a read-only {} subcommand",
                                self.name, sub, self.name
                            ),
                        );
                    }
                }
                Constraint::MutatingFlags(flags) => {
                    if let Some(flag) = util::find_mutating_flag(words, flags) {
                        return needs_approval(
                            ctx,
                            format!("'{}' flag '{}' may mutate state", self.name, flag),
                        );
                    }
                }
                Constraint::MutatingPredicate(predicate) => {
                    if predicate(words) {
                        return needs_approval(
                            ctx,
                            format!("'{}' invocation runs a command", self.name),
                        );
                    }
                }
            }
        }
        ToolDecision::Allow
    }
}

/// Shorthand for a constraint-free read-only command.
macro_rules! ro {
    ($name:literal) => {
        CommandSpec {
            name: $name,
            constraints: &[],
        }
    };
}

/// The catalog: every command the classifier will auto-approve, each with the
/// constraints that keep it read-only. Commands not listed here defer to
/// approval.
pub(super) static READ_ONLY_CATALOG: &[CommandSpec] = &[
    // --- Filesystem info ---
    ro!("pwd"),
    ro!("ls"),
    ro!("stat"),
    ro!("file"),
    ro!("tree"),
    ro!("du"),
    ro!("df"),
    // --- Search ---
    ro!("rg"),
    ro!("grep"),
    ro!("ag"),
    ro!("ack"),
    ro!("fgrep"),
    ro!("egrep"),
    // --- File viewing ---
    ro!("cat"),
    ro!("head"),
    ro!("tail"),
    ro!("less"),
    ro!("more"),
    ro!("bat"),
    // --- Text processing (read-only as commands; writes only via `>`, caught at the redirect layer) ---
    ro!("uniq"),
    ro!("wc"),
    ro!("cut"),
    ro!("tr"),
    ro!("comm"),
    ro!("diff"),
    // --- Output ---
    ro!("echo"),
    ro!("printf"),
    // --- Environment info ---
    ro!("printenv"),
    ro!("which"),
    ro!("type"),
    // --- System info ---
    ro!("date"),
    ro!("uname"),
    ro!("hostname"),
    ro!("whoami"),
    ro!("id"),
    // --- Process info ---
    ro!("ps"),
    ro!("top"),
    ro!("htop"),
    // --- Data processing (read-only) ---
    ro!("jq"),
    // --- Shell builtins (side-effect-free in a one-shot process) ---
    ro!("test"),
    ro!("true"),
    ro!("false"),
    // --- Directory change ---
    // `cd` is read-only only because the runtime shell is a stateless one-shot
    // process: it changes the cwd of a subprocess that dies immediately, so no
    // state persists. A future persistent-session mode must re-evaluate.
    ro!("cd"),
    // --- Agent CLI (approval/management; auth handled by the tagma) ---
    ro!("kallip"),
    // --- Commands whose flags/subcommands can mutate or execute ---
    CommandSpec {
        name: "find",
        constraints: &[Constraint::MutatingFlags(&[
            "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fls", "-fprint", "-fprint0",
            "-fprintf",
        ])],
    },
    CommandSpec {
        name: "sort",
        constraints: &[Constraint::MutatingFlags(&["-o"])],
    },
    CommandSpec {
        name: "yq",
        constraints: &[Constraint::MutatingFlags(&["-i", "--inplace"])],
    },
    CommandSpec {
        name: "env",
        constraints: &[Constraint::MutatingPredicate(env_runs_command)],
    },
    CommandSpec {
        name: "git",
        // `Subcommands` is the primary gate today (`reset`/`clean`/`push` are not
        // listed, so they already defer). The `--hard`/`--keep` flags are redundant
        // now but guard against a future addition of `reset` to the subcommand list.
        constraints: &[
            Constraint::Subcommands(&["log", "status", "diff", "show", "blame"]),
            Constraint::MutatingFlags(&["--hard", "--keep"]),
        ],
    },
];

// ---------------------------------------------------------------------------
// Structural constants (consumed by sibling modules)
// ---------------------------------------------------------------------------

/// Shell interpreters whose `-c`/eval argument must be re-parsed (see
/// `delegate.rs`).
pub(super) static SHELL_INTERPRETERS: &[&str] = &[
    "bash", "sh", "dash", "zsh", "ksh", "csh", "tcsh", "fish", "ash", "busybox",
];

/// Commands that evaluate a string as shell (`delegate.rs`).
pub(super) static EVAL_COMMANDS: &[&str] = &["eval", "exec", "source", "."];

/// Flags that introduce a command string for an interpreter (`delegate.rs`).
pub(super) static COMMAND_STRING_FLAGS: &[&str] = &["-c"];

/// Environment variables whose override can alter security-critical behavior
/// (`helpers.rs` assignment check).
pub(super) static SENSITIVE_ENV_VARS: &[&str] = &[
    "PATH",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "PYTHONPATH",
    "HOME",
    "SHELL",
    "IFS",
];

/// One builtin denylist rule. A name-only rule (`subcommand: None`) denies every
/// invocation of the command (sed/awk/…). A structured rule additionally
/// matches a subcommand and flag set, so one command name can be denied only
/// in its history-surgery shapes (`git rebase -i`) while its benign shapes
/// pass through to the normal path.
pub(super) struct DenyRule {
    pub(super) name: &'static str,
    /// `None` = name-only rule. `Some(s)` = applies only when the literal
    /// subcommand is `s`.
    pub(super) subcommand: Option<&'static str>,
    /// Any-of flag list: a match denies. Empty = the whole subcommand. Long
    /// flags compare the name before `=`; single-char entries also match
    /// inside short clusters (`-fi`).
    pub(super) deny_flags: &'static [&'static str],
    /// Read-only escapes checked first: if any is present, no match.
    pub(super) except_flags: &'static [&'static str],
    pub(super) reason: &'static str,
}
/// `git`'s value-taking global flags: skip the following token while locating
/// the subcommand.
const GIT_VALUE_GLOBAL_FLAGS: &[&str] = &[
    "-c",
    "-C",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
];
/// `git`'s boolean global flags: skipped without consuming a value.
const GIT_BOOL_GLOBAL_FLAGS: &[&str] = &[
    "--version",
    "--help",
    "--no-pager",
    "-p",
    "--paginate",
    "--no-replace-objects",
    "--bare",
    "--no-optional-locks",
    "--literal-pathspecs",
    "--glob-pathspecs",
    "--noglob-pathspecs",
    "--icase-pathspecs",
];
/// The builtin denylist. Checked as a hard floor at the top of `apply_override`
/// (`walker.rs`), before per-agent overrides, so no rule can be widened. The
/// structured git rules are fail-closed: a subcommand or flag shape that cannot
/// be statically verified (expansions, unknown global flags) denies with the
/// rule's reason — over-deny is the safe direction for a hard floor, and the
/// reasons tell the agent how to rewrite the call literally. Direct invocations
/// only — wrapped forms (`busybox sed`, `env sed`, `nice sed`) key off the
/// outer command name and bypass a name-only check (defense-in-depth, the same
/// gap the catalog allow-list has).
pub(super) static BUILTIN_DENYLIST: &[DenyRule] = &[
    DenyRule {
        name: "sed",
        subcommand: None,
        deny_flags: &[],
        except_flags: &[],
        reason: "silent substitution; the scope of changes is hard to confirm; make changes manually",
    },
    DenyRule {
        name: "awk",
        subcommand: None,
        deny_flags: &[],
        except_flags: &[],
        reason: "complex, error-prone syntax; a misread misleads decisions; use a more targeted tool",
    },
    DenyRule {
        name: "ed",
        subcommand: None,
        deny_flags: &[],
        except_flags: &[],
        reason: "line-editor scripts mutate files silently; hard to confirm the scope of changes",
    },
    DenyRule {
        name: "ex",
        subcommand: None,
        deny_flags: &[],
        except_flags: &[],
        reason: "line-editor (ex mode) scripts mutate files silently; hard to confirm the scope of changes",
    },
    DenyRule {
        name: "git",
        subcommand: Some("rebase"),
        deny_flags: &["-i", "--interactive"],
        except_flags: &[],
        reason: "interactive rebase rewrites history; that is operator-only (re-run with flags written literally if this was a false positive)",
    },
    DenyRule {
        name: "git",
        subcommand: Some("push"),
        deny_flags: &["-f", "--force", "--force-with-lease"],
        except_flags: &[],
        reason: "force push is operator-only: it publishes rewritten history (re-run with flags written literally if this was a false positive)",
    },
    DenyRule {
        name: "git",
        subcommand: Some("filter-branch"),
        deny_flags: &[],
        except_flags: &[],
        reason: "filter-branch rewrites history wholesale; that is operator-only",
    },
    DenyRule {
        name: "git",
        subcommand: Some("config"),
        deny_flags: &[],
        except_flags: &[
            "--get",
            "--get-all",
            "--get-regexp",
            "--list",
            "-l",
            "--get-color",
            "--get-colorbool",
        ],
        reason: "git config writes are operator-only: they could plant an alias around these rules; reads need a --get/--list flag, written literally",
    },
];

// ---------------------------------------------------------------------------
// Rendering (for the agent self-query tool / CLI)
// ---------------------------------------------------------------------------

/// Render a spec's constraints to human-readable strings.
pub(super) fn summarize_constraints(constraints: &[Constraint]) -> Vec<String> {
    constraints
        .iter()
        .map(|c| match c {
            Constraint::Subcommands(subs) => {
                format!("read-only subcommands: {}", subs.join(", "))
            }
            Constraint::MutatingFlags(flags) => {
                format!("asks on flags: {}", flags.join(", "))
            }
            Constraint::MutatingPredicate(_) => "subject to a runtime guard".to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Lookup
// ---------------------------------------------------------------------------

/// Look up `name` in `catalog` and apply its constraints.
///
/// Returns `None` when the command is absent from the catalog (the caller
/// decides its fate, factoring in exec-policy overrides). A `Some` verdict
/// already reflects the spec's constraints (e.g. `find -delete` → Ask/Deny).
pub(super) fn classify_named_command(
    ctx: &ClassifyCtx<'_>,
    name: &str,
    words: &[Node],
) -> Option<ToolDecision> {
    let catalog = ctx.catalog();
    catalog
        .iter()
        .find(|spec| spec.name == name)
        .map(|spec| spec.classify(ctx, words))
}

// ---------------------------------------------------------------------------
// Structured denylist matching
// ---------------------------------------------------------------------------

/// Whether any builtin denylist rule matches this invocation, and its reason.
///
/// Name-only rules match on the command name alone. Structured rules (git)
/// locate the literal subcommand — skipping git's global flags — then apply
/// flag matching; a shape that cannot be verified statically denies
/// fail-closed. `words` are the command's argv nodes (`words[0]` = name).
pub(super) fn builtin_deny_reason(name: &str, words: &[Node]) -> Option<&'static str> {
    let rules = BUILTIN_DENYLIST
        .iter()
        .filter(|r| r.name.eq_ignore_ascii_case(name));
    let mut structured: Vec<&DenyRule> = Vec::new();
    for rule in rules {
        match rule.subcommand {
            None => return Some(rule.reason),
            Some(_) => structured.push(rule),
        }
    }
    if structured.is_empty() {
        return None;
    }
    let sub = match locate_git_subcommand(words) {
        // Unverifiable shape (expansion / unknown flag): fail-closed. The
        // generic reason below names every gated shape class so the agent
        // knows how to recover (rewrite with literal flags) whatever the
        // ambiguous shape actually was.
        None => return Some(UNVERIFIABLE_GIT_SHAPE_REASON),
        // Bare `git` (or only global flags): no subcommand, nothing structured
        // can match.
        Some(None) => return None,
        Some(Some(sub)) => sub,
    };
    for rule in structured {
        if rule.subcommand != Some(sub) {
            continue;
        }
        // Strict: any non-literal token after the subcommand is an
        // unverifiable flag shape — deny rather than guess.
        if words
            .iter()
            .skip(2)
            .any(|w| util::word_literal_value(w).is_none())
        {
            return Some(rule.reason);
        }
        if (rule.deny_flags.is_empty() || flags_match(words, rule.deny_flags))
            && !flags_match(words, rule.except_flags)
        {
            return Some(rule.reason);
        }
    }
    None
}

/// Locate git's subcommand word: the first literal non-flag token after
/// skipping known global flags. `Some(None)` = ran out of words (bare `git`);
/// `None` = unverifiable (expansion or unknown flag before the subcommand).
fn locate_git_subcommand(words: &[Node]) -> Option<Option<&str>> {
    let mut iter = words.iter().skip(1);
    while let Some(word) = iter.next() {
        let Some(raw) = util::word_literal_value(word) else {
            return None; // expansion before the subcommand — unverifiable
        };
        let token = util::strip_surrounding_quotes(raw);
        if GIT_VALUE_GLOBAL_FLAGS.contains(&token) {
            let Some(next) = iter.next() else {
                return Some(None);
            };
            util::word_literal_value(next)?; // expansion as a flag value — unverifiable
        } else if GIT_BOOL_GLOBAL_FLAGS.contains(&token) {
            // boolean global — skip
        } else if token.starts_with('-') {
            return None; // unknown flag shape before the subcommand
        } else {
            return Some(Some(token));
        }
    }
    Some(None) // bare `git` / only global flags
}

/// Whether any of `flags` matches the invocation's literal flag tokens.
/// Long flags compare the name before `=`; single-char short flags also match
/// inside boolean clusters (`-fi`) — but only for cluster tokens of at most
/// two flag chars, because glued short-with-value forms (`-Xtheirs`,
/// `-o<value>`) are values, not flag clusters, and a char scan there would
/// false-match (deny) valid invocations. A non-literal token never matches
/// (the caller's fail-closed path handles expansions).
fn flags_match(words: &[Node], flags: &[&str]) -> bool {
    words.iter().skip(1).any(|word| {
        let Some(raw) = util::word_literal_value(word) else {
            return false;
        };
        let token = util::strip_surrounding_quotes(raw);
        flags.iter().any(|f| {
            if f.starts_with("--") {
                token.split('=').next() == Some(*f)
            } else {
                token == *f
                    || (token.starts_with('-')
                        && !token.starts_with("--")
                        && f.len() == 2
                        && token.len() <= 3
                        && token
                            .chars()
                            .skip(1)
                            .any(|c| c == f.chars().nth(1).unwrap()))
            }
        })
    })
}

/// Reason for git invocations whose subcommand/flag shape cannot be verified
/// statically (expansions, unknown pre-subcommand flags): naming the gated
/// classes inline keeps the recovery instruction accurate no matter which
/// rule the ambiguous shape resembled.
const UNVERIFIABLE_GIT_SHAPE_REASON: &str = "git subcommand or flag shape could not be verified statically - this floor gates rebase -i, force push, filter-branch, and config writes; rewrite the command with literal flags (or have the operator run it)";
// ---------------------------------------------------------------------------
// `env` predicate
// ---------------------------------------------------------------------------

/// Whether an `env` invocation runs a command rather than merely printing or
/// setting the environment.
///
/// `env` with no command prints the environment (read-only). It runs a command
/// when given a non-flag operand that is not a `NAME=VALUE` assignment. `env -S
/// '<script>'` is special: GNU coreutils word-splits and *executes* the string,
/// so any presence of `-S`/`--split-string` is treated as executing
/// (fail-closed). Value-consuming flags (`-u`, `-C`, `--unset`, `--chdir`) have
/// their argument skipped so e.g. `env -u PATH` is not mistaken for running a
/// command named `PATH`.
fn env_runs_command(words: &[Node]) -> bool {
    let mut iter = words.iter().skip(1);
    let mut after_dashdash = false;
    while let Some(word) = iter.next() {
        let Some(token) = util::word_literal_value(word) else {
            // Non-literal operand — cannot prove it is an assignment → fail-closed.
            return true;
        };
        if after_dashdash {
            // After `--`, every remaining token is positional. Scan them all: a
            // command is present if ANY operand is not an assignment (e.g.
            // `env -- FOO=bar rm` runs `rm`).
            if !is_assignment(token) {
                return true;
            }
            continue;
        }
        match token {
            "--" => after_dashdash = true,
            // `-S`/`--split-string[=...]` executes its argument as a command line.
            t if t == "-S" || t == "--split-string" || t.starts_with("--split-string=") => {
                return true;
            }
            // Value-consuming flags: skip the next token as their argument.
            "-u" | "-C" | "--unset" | "--chdir" => {
                iter.next();
            }
            t if t.starts_with('-') => {}
            t => {
                if !is_assignment(t) {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether `token` is a `NAME=VALUE` environment assignment.
fn is_assignment(token: &str) -> bool {
    let Some(idx) = token.find('=') else {
        return false;
    };
    if idx == 0 {
        return false;
    }
    let name = &token[..idx];
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty name");
    (first.is_ascii_alphabetic() || first == '_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod env_predicate_tests {
    use super::env_runs_command;

    /// Build the `words` of an `env <args>` invocation by parsing it, so the
    /// predicate is tested against real rable word nodes rather than hand-built
    /// AST.
    fn words(args: &str) -> Vec<rable::Node> {
        let nodes = rable::parse(&format!("env {args}"), false).unwrap();
        match &nodes[0].kind {
            rable::NodeKind::Command { words, .. } => words.clone(),
            _ => unreachable!("env <args> parses to a single command"),
        }
    }

    #[test]
    fn bare_env_prints_environment() {
        assert!(!env_runs_command(&words("")));
    }

    #[test]
    fn env_with_assignment_only_is_read_only() {
        assert!(!env_runs_command(&words("FOO=bar BAZ=qux")));
    }

    #[test]
    fn env_running_a_command_is_detected() {
        assert!(env_runs_command(&words("FOO=bar ls")));
    }

    #[test]
    fn env_unset_flag_argument_not_mistaken_for_command() {
        assert!(!env_runs_command(&words("-u PATH FOO=bar")));
    }

    #[test]
    fn env_chdir_flag_argument_not_mistaken_for_command() {
        assert!(!env_runs_command(&words("-C /tmp")));
    }

    #[test]
    fn env_split_string_executes() {
        assert!(env_runs_command(&words("-S rm -rf /")));
    }

    #[test]
    fn env_split_string_attached_form_executes() {
        assert!(env_runs_command(&words("--split-string=rm")));
    }

    #[test]
    fn env_after_dashdash_runs_command() {
        assert!(env_runs_command(&words("-- ls")));
    }
}
