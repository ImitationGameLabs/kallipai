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
