//! The read-only command catalog — the single source of truth for which shell
//! commands the classifier treats as side-effect-free.
//!
//! A command absent from [`READ_ONLY_CATALOG`] is never auto-approved:
//! [`classify_named_command`] returns [`Safety::NeedsApproval`] for it. "What is
//! allowed" is therefore an explicit, auditable list rather than an implicit
//! string fallback. Mutating/dangerous commands (`rm`, `sudo`, `dd`, …) are not
//! listed and so defer to approval by default — there is no separate "dangerous
//! list" to keep in sync.

use rable::Node;

use super::Safety;
use super::util;

/// A constraint that can downgrade an otherwise-allowed command to
/// [`Safety::NeedsApproval`].
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
    /// Returns [`Safety::ReadOnly`] only if no constraint trips.
    fn classify(&self, words: &[Node]) -> Safety {
        for constraint in self.constraints {
            match constraint {
                Constraint::Subcommands(safe) => {
                    // words[1] is the subcommand. Missing, non-literal, or
                    // unlisted → defer to approval. words.get(1) avoids indexing
                    // a bare command (e.g. `git` with no subcommand).
                    let sub = words.get(1).and_then(util::word_literal_value);
                    if !sub.is_some_and(|s| safe.contains(&s)) {
                        return Safety::NeedsApproval;
                    }
                }
                Constraint::MutatingFlags(flags) => {
                    if util::has_any_flag(words, flags) {
                        return Safety::NeedsApproval;
                    }
                }
                Constraint::MutatingPredicate(predicate) => {
                    if predicate(words) {
                        return Safety::NeedsApproval;
                    }
                }
            }
        }
        Safety::ReadOnly
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
    // --- Agent CLI (approval/management; auth handled by the daemon) ---
    ro!("just-agent"),
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

// ---------------------------------------------------------------------------
// Lookup
// ---------------------------------------------------------------------------

/// Look up `name` in `catalog` and apply its constraints.
///
/// Commands absent from the catalog are never auto-approved.
pub(super) fn classify_named_command(
    catalog: &'static [CommandSpec],
    name: &str,
    words: &[Node],
) -> Safety {
    match catalog.iter().find(|spec| spec.name == name) {
        Some(spec) => spec.classify(words),
        None => Safety::NeedsApproval,
    }
}

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
