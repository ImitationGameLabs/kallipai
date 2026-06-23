//! Stateless, self-contained shell-command safety classifier.
//!
//! Encapsulated behind [`Classifier`] and returning its own [`Safety`] decision
//! type, this module depends only on the `rable` parser — it deliberately does
//! not import the runtime's `policy::ToolDecision`. The policy layer
//! (`AgentPolicy`) maps `Safety` to `ToolDecision` at a single boundary
//! (see `policy::agent`).

mod catalog;
mod delegate;
mod helpers;
mod util;
mod walker;

#[cfg(test)]
mod tests;

use just_agent_common::policy::{ExecDecision, ExecPolicy};
use serde::Serialize;

use catalog::CommandSpec;

/// Authorization decision produced by the classifier.
///
/// Intentionally decoupled from [`super::ToolDecision`]: the classifier reasons
/// about *read-only-ness* of a shell command, not about the runtime's
/// allow/ask/deny enforcement. The policy layer maps these one-to-one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Safety {
    /// No mutation or code execution was detected.
    ReadOnly,
    /// The command may mutate state or execute code; defer to a human.
    NeedsApproval,
    /// The classifier declines to greenlight (e.g. unparseable input, or a
    /// hard-refused pattern such as piping a download into a shell).
    Reject { reason: String },
}

/// Read-only inputs for one classification pass: the static catalog plus the
/// per-agent exec-policy overrides (borrowed for the duration of the call).
///
/// `Copy` so it threads through the recursive walker by value — no lifetime
/// juggling beyond the single `<'a>` tying it to the borrowed overrides.
///
/// Visibility is scoped to the classifier module (and its children) so the
/// internal `CommandSpec` type does not leak to the policy layer.
#[derive(Clone, Copy)]
pub(in crate::policy::classifier) struct ClassifyCtx<'a> {
    catalog: &'static [CommandSpec],
    overrides: &'a ExecPolicy,
}

impl<'a> ClassifyCtx<'a> {
    pub(in crate::policy::classifier) fn catalog(&self) -> &'static [CommandSpec] {
        self.catalog
    }

    /// The exec-policy override for a command name, if any.
    pub(in crate::policy::classifier) fn override_for(&self, name: &str) -> Option<ExecDecision> {
        self.overrides.override_for(name)
    }
}

/// Encapsulates the read-only command catalog and classifies shell commands.
///
/// Owns its catalog so the policy that governs classification lives inside the
/// object rather than in a module global. The default catalog is the
/// compile-time `READ_ONLY_CATALOG`.
#[derive(Clone, Copy, Debug)]
pub struct Classifier {
    catalog: &'static [CommandSpec],
}

impl Classifier {
    /// The default classifier, backed by the built-in read-only catalog.
    pub const DEFAULT: Self = Self {
        catalog: catalog::READ_ONLY_CATALOG,
    };

    /// Parse and classify a shell command with no per-agent overrides.
    /// Fail-closed: unparseable input is [`Safety::Reject`].
    pub fn classify(&self, command: &str) -> Safety {
        self.classify_with(command, &ExecPolicy::default())
    }

    /// Parse and classify a shell command, applying `overrides` per simple
    /// command. The override is applied per simple command inside the walker:
    /// `Allow` widens only commands absent from the catalog; for listed commands
    /// the catalog verdict (constraints included) is authoritative.
    pub fn classify_with(&self, command: &str, overrides: &ExecPolicy) -> Safety {
        let ctx = ClassifyCtx {
            catalog: self.catalog,
            overrides,
        };
        walker::classify_command(&ctx, command)
    }
}

// ---------------------------------------------------------------------------
// Read accessors for external renderers (agent self-tool, CLI, HTTP).
// ---------------------------------------------------------------------------

/// Whether `name` is a listed read-only command in the default catalog.
///
/// Used by the exec-policy strictness check as the baseline resolver: a listed
/// command's baseline is `Allow`, an absent command's is `Ask`.
pub fn catalog_contains(name: &str) -> bool {
    catalog::READ_ONLY_CATALOG
        .iter()
        .any(|spec| spec.name == name)
}

/// The baseline exec decision for a command name: `Allow` if listed in the
/// read-only catalog, else `Ask`. Passed to [`ExecPolicy::validate_at_least_as_strict_as`]
/// so strictness compares *effective* (override-or-baseline) decisions.
pub fn exec_baseline(name: &str) -> ExecDecision {
    if catalog_contains(name) {
        ExecDecision::Allow
    } else {
        ExecDecision::Ask
    }
}

/// Whether `name` may be used as an exec-policy override key.
///
/// Shell interpreter / eval command names (`bash`, `sh`, `eval`, `source`, `.`,
/// …) are rejected: their invocations are re-parsed by interpreter delegation
/// *before* the override site, so an override on them would silently never apply
/// and mislead the user.
pub fn is_valid_override_key(name: &str) -> std::result::Result<(), String> {
    if name.trim().is_empty() {
        return Err("override key must not be empty or whitespace".into());
    }
    if catalog::SHELL_INTERPRETERS.contains(&name) || catalog::EVAL_COMMANDS.contains(&name) {
        return Err(format!(
            "cannot override '{name}': shell interpreter/eval invocations are re-parsed, \
             so an override would never apply"
        ));
    }
    Ok(())
}

/// One row of the read-only catalog, rendered for external display.
#[derive(Serialize, Clone)]
pub struct CatalogEntry {
    pub name: &'static str,
    pub constraints: Vec<String>,
}

/// Summarize every command in the default catalog (name + constraint
/// descriptions). Single source of truth for the agent self-query tool and CLI.
pub fn default_catalog_summary() -> Vec<CatalogEntry> {
    catalog::READ_ONLY_CATALOG
        .iter()
        .map(|spec| CatalogEntry {
            name: spec.name,
            constraints: catalog::summarize_constraints(spec.constraints),
        })
        .collect()
}

/// Structural shell rules that apply regardless of command identity: each pair
/// is `(rule, effect)`. Reported by the agent self-query tool.
///
/// NOTE: keep in sync with the walker's per-node decisions (`walker.rs`). These
/// are stable rules, but a new structural check added to the walker should be
/// reflected here too.
pub const STRUCTURAL_RULES: &[(&str, &str)] = &[
    ("command not in the read-only catalog", "asks"),
    (
        "composition (&& ; || |)",
        "read-only iff every component is read-only",
    ),
    ("background operator &", "asks"),
    ("download piped to a shell (curl|sh)", "rejects"),
    ("output redirect (> >>)", "asks"),
    (
        "sensitive env var assignment (PATH, LD_PRELOAD, ...)",
        "asks",
    ),
];
