//! Policy decision and tool policy types.
//!
//! Shared between the runtime policy engine and the daemon API/config.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Decision for a tool in the policy.
///
/// Ordering (via derived `Ord`): Allow < Classify < Ask < Deny.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Classify,
    Ask,
    Deny,
}

impl std::fmt::Display for PolicyDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Allow => "allow",
            Self::Classify => "classify",
            Self::Ask => "ask",
            Self::Deny => "deny",
        })
    }
}

impl std::str::FromStr for PolicyDecision {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "allow" => Ok(Self::Allow),
            "classify" => Ok(Self::Classify),
            "ask" => Ok(Self::Ask),
            "deny" => Ok(Self::Deny),
            _ => Err(format!(
                "invalid policy decision '{s}' (expected allow/ask/deny/classify)"
            )),
        }
    }
}

/// Per-agent tool security policy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolPolicy {
    pub default: PolicyDecision,
    pub tools: BTreeMap<String, PolicyDecision>,
}

impl ToolPolicy {
    pub fn new(default: PolicyDecision) -> Self {
        Self {
            default,
            tools: BTreeMap::new(),
        }
    }

    /// Look up the decision for a tool name.
    pub fn decision_for(&self, tool_name: &str) -> PolicyDecision {
        self.tools.get(tool_name).copied().unwrap_or(self.default)
    }

    /// Validate that this policy is at least as strict as `other`.
    /// Checks the union of both maps' keys plus the default.
    pub fn validate_at_least_as_strict_as(&self, other: &ToolPolicy) -> Result<(), Vec<String>> {
        let mut violations = Vec::new();

        if self.default < other.default {
            violations.push(format!(
                "default {} is less strict than parent's {}",
                self.default, other.default,
            ));
        }

        let all_keys: std::collections::BTreeSet<&String> =
            self.tools.keys().chain(other.tools.keys()).collect();

        for key in &all_keys {
            let mine = self.decision_for(key);
            let theirs = other.decision_for(key);
            if mine < theirs {
                violations.push(format!(
                    "{key}: {} is less strict than parent's {}",
                    mine, theirs,
                ));
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}

/// Per-command decision for the `bash_exec` exec-policy override layer.
///
/// Intentionally distinct from [`PolicyDecision`]: `Classify` has no meaning for
/// a per-command shell override (the catalog *is* the classify step), and this
/// lattice's `Ord` (Allow < Ask < Deny) drives the monotonic-strictness check in
/// [`ExecPolicy::validate_at_least_as_strict_as`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecDecision {
    Allow,
    Ask,
    Deny,
}

impl std::fmt::Display for ExecDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        })
    }
}

impl std::str::FromStr for ExecDecision {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "allow" => Ok(Self::Allow),
            "ask" => Ok(Self::Ask),
            "deny" => Ok(Self::Deny),
            _ => Err(format!(
                "invalid exec decision '{s}' (expected allow/ask/deny)"
            )),
        }
    }
}

/// Per-agent `bash_exec` command-policy overrides layered on the static read-only
/// catalog. An effective decision for a command is `overrides.get(name)` if
/// present, else the catalog's baseline verdict (supplied by the caller, since
/// the catalog lives in the runtime crate).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExecPolicy {
    #[serde(default)]
    pub overrides: BTreeMap<String, ExecDecision>,
}

impl ExecPolicy {
    /// Validate that `self` is at least as strict as `other` (a parent).
    ///
    /// Compares *effective* decisions over the union of override keys, using
    /// `baseline(name)` as the fallback for a key absent from a side's map.
    /// `baseline(name)` is the catalog's name-level verdict (`Allow` if listed,
    /// `Ask` if absent) — a per-name least-strict value, NOT the per-invocation
    /// verdict. Per-invocation constraint verdicts (e.g. `find -delete`) remain
    /// authoritative at classify time and are invisible to this comparison, which
    /// only needs the name-level lattice to enforce monotonicity. The catalog is
    /// not visible from this crate, so the baseline is supplied by the caller.
    ///
    /// A parent **narrowing** override (e.g. `ls -> ask`) is viral: a child that
    /// drops it inherits the looser catalog baseline and is rejected. A parent
    /// **widening** override (`cargo -> allow` on an absent command) is not viral:
    /// a child may stay stricter (catalog default). This mirrors
    /// [`ToolPolicy::validate_at_least_as_strict_as`].
    pub fn validate_at_least_as_strict_as(
        &self,
        other: &ExecPolicy,
        baseline: impl Fn(&str) -> ExecDecision,
    ) -> Result<(), Vec<String>> {
        let effective = |policy: &ExecPolicy, name: &str| -> ExecDecision {
            policy
                .overrides
                .get(name)
                .copied()
                .unwrap_or_else(|| baseline(name))
        };

        let mut violations = Vec::new();
        let names: std::collections::BTreeSet<&str> = self
            .overrides
            .keys()
            .chain(other.overrides.keys())
            .map(String::as_str)
            .collect();
        for name in names {
            let mine = effective(self, name);
            let theirs = effective(other, name);
            if mine < theirs {
                violations.push(format!(
                    "{name}: {mine} is less strict than parent's {theirs}",
                ));
            }
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    /// Look up the override decision for a command name, if any. Returns the
    /// raw override only (not the catalog-baseline fallback) — hence `override_for`
    /// rather than a "decision" that implies an effective verdict.
    pub fn override_for(&self, name: &str) -> Option<ExecDecision> {
        self.overrides.get(name).copied()
    }

    /// Lowercase every override key in place. Command names are matched
    /// case-insensitively (the classifier lowercases `cmd_name`), so mixed-case
    /// keys would silently never match.
    pub fn lowercase_keys(&mut self) {
        let normalized: BTreeMap<String, ExecDecision> = self
            .overrides
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), *v))
            .collect();
        self.overrides = normalized;
    }
}

#[cfg(test)]
mod exec_policy_tests {
    use super::{ExecDecision, ExecPolicy};
    use ExecDecision::*;

    /// Baseline resolver mirroring the runtime's `classifier::exec_baseline`:
    /// listed commands → Allow, absent → Ask. `ls`/`find` are "listed"; `cargo`/`rm`
    /// are "absent" in this test fixture.
    fn baseline(name: &str) -> ExecDecision {
        match name {
            "ls" | "find" => Allow,
            _ => Ask,
        }
    }

    fn policy(pairs: &[(&str, ExecDecision)]) -> ExecPolicy {
        let mut e = ExecPolicy::default();
        for (k, v) in pairs {
            e.overrides.insert((*k).to_string(), *v);
        }
        e
    }

    #[test]
    fn child_matching_parent_is_accepted() {
        let parent = policy(&[("ls", Ask)]);
        let child = policy(&[("ls", Ask)]);
        assert!(
            child
                .validate_at_least_as_strict_as(&parent, baseline)
                .is_ok()
        );
        // Stricter child is fine too.
        let stricter = policy(&[("ls", Deny)]);
        assert!(
            stricter
                .validate_at_least_as_strict_as(&parent, baseline)
                .is_ok()
        );
    }

    #[test]
    fn child_dropping_parent_narrowing_is_rejected() {
        // Parent narrows `ls` (baseline Allow) to Ask. Child with no override
        // inherits baseline Allow → less strict → violation.
        let parent = policy(&[("ls", Ask)]);
        let child = ExecPolicy::default();
        assert!(
            child
                .validate_at_least_as_strict_as(&parent, baseline)
                .is_err()
        );
    }

    #[test]
    fn parent_widening_is_not_viral() {
        // Parent widens `cargo` (baseline Ask) to Allow. Child with no override
        // inherits baseline Ask (stricter) → fine.
        let parent = policy(&[("cargo", Allow)]);
        let child = ExecPolicy::default();
        assert!(
            child
                .validate_at_least_as_strict_as(&parent, baseline)
                .is_ok()
        );
    }

    #[test]
    fn child_widening_beyond_baseline_is_rejected() {
        // Parent silent on cargo (baseline Ask). Child sets cargo→Allow → less
        // strict than parent's effective baseline → violation.
        let parent = ExecPolicy::default();
        let child = policy(&[("cargo", Allow)]);
        assert!(
            child
                .validate_at_least_as_strict_as(&parent, baseline)
                .is_err()
        );
    }

    #[test]
    fn lowercase_keys_normalizes() {
        let mut p = policy(&[("LS", Allow), ("Cargo", Ask)]);
        p.lowercase_keys();
        assert_eq!(p.overrides.get("ls"), Some(&Allow));
        assert_eq!(p.overrides.get("cargo"), Some(&Ask));
        assert!(!p.overrides.contains_key("LS"));
    }
}
