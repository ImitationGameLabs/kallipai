//! Preset-aware shell-command classifier.
//!
//! Encapsulated behind [`Classifier`], this module parses a shell command with the
//! `rable` parser and returns a final [`super::ToolDecision`] directly — there is
//! no intermediate "safety" type and no separate mapping layer. The decision
//! depends on the static catalog, the per-agent [`ExecPolicy`] overrides, and the
//! daemon-global [`PolicyPreset`] (which selects the rule-set: how unclassified
//! commands resolve and whether the denylist applies).

mod catalog;
mod delegate;
mod helpers;
mod util;
mod walker;

#[cfg(test)]
mod tests;

use kallip_common::policy::{ExecDecision, ExecOverride, ExecPolicy, PolicyPreset};
use serde::Serialize;

use super::ToolDecision;
use catalog::CommandSpec;

/// Read-only inputs for one classification pass: the static catalog, the preset
/// rule-set, and the per-agent exec-policy overrides (borrowed for the call).
///
/// `Copy` so it threads through the recursive walker by value — no lifetime
/// juggling beyond the single `<'a>` tying it to the borrowed overrides.
///
/// Visibility is scoped to the classifier module (and its children) so the
/// internal `CommandSpec` type does not leak to the policy layer.
#[derive(Clone, Copy)]
pub(in crate::policy::classifier) struct ClassifyCtx<'a> {
    catalog: &'static [CommandSpec],
    preset: PolicyPreset,
    overrides: &'a ExecPolicy,
}

impl<'a> ClassifyCtx<'a> {
    pub(in crate::policy::classifier) fn catalog(&self) -> &'static [CommandSpec] {
        self.catalog
    }

    pub(in crate::policy::classifier) fn preset(&self) -> PolicyPreset {
        self.preset
    }

    /// The exec-policy override for a command name, if any.
    pub(in crate::policy::classifier) fn override_for(&self, name: &str) -> Option<&ExecOverride> {
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

    /// Parse and classify a shell command, applying `overrides` per simple command
    /// under the `preset` rule-set. Returns the final [`ToolDecision`] directly.
    ///
    /// Fail-closed: unparseable / empty input is `Deny` regardless of preset.
    pub fn classify_with(
        &self,
        command: &str,
        overrides: &ExecPolicy,
        preset: PolicyPreset,
    ) -> ToolDecision {
        let ctx = ClassifyCtx {
            catalog: self.catalog,
            preset,
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
/// and mislead the user. Builtin-denied names ([`builtin_deny_reason`]) are also
/// rejected: the classifier refuses them unconditionally, so an override could
/// only mislead (it would be inert).
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
    if builtin_deny_reason(name).is_some() {
        return Err(format!(
            "cannot override '{name}': it is builtin-denied and cannot be widened"
        ));
    }
    Ok(())
}

/// The curated reason a command is builtin-denied, if any. Matched
/// case-insensitively (command names are lowercased before the override site, but
/// this keeps the contract consistent with the rest of the classifier).
pub fn builtin_deny_reason(name: &str) -> Option<&'static str> {
    catalog::BUILTIN_DENYLIST
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, reason)| *reason)
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

/// One row of the builtin denylist, rendered for external display.
#[derive(Serialize, Clone)]
pub struct DenylistEntry {
    pub name: &'static str,
    pub reason: &'static str,
}

/// Summarize every builtin-denied command (name + reason). Surfaced to the agent
/// via the `exec_policy` self-query tool — denylisted names never appear in
/// `overrides`, so this is the only way the agent learns them proactively.
pub fn builtin_denylist_summary() -> Vec<DenylistEntry> {
    catalog::BUILTIN_DENYLIST
        .iter()
        .map(|(name, reason)| DenylistEntry { name, reason })
        .collect()
}

/// Structural shell rules that apply regardless of command identity: each pair
/// is `(rule, effect)`. Reported by the agent self-query tool.
///
/// NOTE: keep in sync with the walker's per-node decisions (`walker.rs`). These
/// are stable rules, but a new structural check added to the walker should be
/// reflected here too.
pub const STRUCTURAL_RULES: &[(&str, &str)] = &[
    (
        "command not in the read-only catalog",
        "ask under default, allow under auto (allow-all allows everything)",
    ),
    (
        "composition (&& ; || |)",
        "read-only iff every component is read-only",
    ),
    (
        "background operator &",
        "ask under default, allow under auto",
    ),
    ("download piped to a shell (curl|sh)", "deny"),
    (
        "builtin denylist (sed/awk/ed/ex)",
        "deny with a curated reason",
    ),
    (
        "output redirect (> >> >| <> &> &>>)",
        "ask under default, allow under auto, except to /dev/null (pure sink)",
    ),
    (
        "fd duplication/close (2>&1, >&-)",
        "read-only (no file opened)",
    ),
    (
        "sensitive env var assignment (PATH, LD_PRELOAD, ...)",
        "ask under default, allow under auto",
    ),
];
