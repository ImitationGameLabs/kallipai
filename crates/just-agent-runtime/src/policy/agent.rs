//! Agent policy: per-tool authorization decisions.

use std::sync::{Arc, RwLock};

use anyhow::Result;
use just_agent_common::policy::{PolicyDecision, ToolPolicy};

use just_agent_shell::stateless::tools::{BashExecArgs, names};

use super::ToolDecision;
use super::classifier;

/// Policy layer that gates every tool call.
///
/// Wraps a shared `ToolPolicy` that can be updated at runtime by the daemon.
#[derive(Clone, Debug)]
pub struct AgentPolicy {
    policy: Arc<RwLock<ToolPolicy>>,
}

impl AgentPolicy {
    pub fn new(policy: Arc<RwLock<ToolPolicy>>) -> Self {
        Self { policy }
    }

    pub fn evaluate(&self, tool_name: &str, args_json: &str) -> Result<ToolDecision> {
        let decision = self
            .policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .decision_for(tool_name);
        match decision {
            PolicyDecision::Allow => Ok(ToolDecision::Allow),
            PolicyDecision::Deny => Ok(ToolDecision::Deny {
                reason: format!("{tool_name} denied by policy"),
            }),
            PolicyDecision::Ask => Ok(ToolDecision::Ask),
            PolicyDecision::Classify => {
                if tool_name == names::BASH_EXEC {
                    let args: BashExecArgs = serde_json::from_str(args_json)?;
                    Ok(classifier::classify_command(&args.command))
                } else {
                    Ok(ToolDecision::Ask)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_tool_policy;

    fn make_policy() -> AgentPolicy {
        AgentPolicy::new(Arc::new(RwLock::new(default_tool_policy())))
    }

    #[test]
    fn background_read_is_allowed() {
        let policy = make_policy();
        assert!(matches!(
            policy.evaluate(names::BG_READ, "{}").unwrap(),
            ToolDecision::Allow
        ));
    }

    #[test]
    fn classify_delegates_to_classifier_for_bash_exec() {
        let policy = make_policy();
        // "ls" is on the read-only allowlist → Allow via classifier.
        let decision = policy
            .evaluate(names::BASH_EXEC, r#"{"command":"ls"}"#)
            .unwrap();
        assert!(matches!(decision, ToolDecision::Allow));
    }

    #[test]
    fn unknown_tool_asks() {
        let policy = make_policy();
        let decision = policy.evaluate("some_new_tool", "{}").unwrap();
        assert!(matches!(decision, ToolDecision::Ask));
    }

    #[test]
    fn policy_update_takes_effect() {
        let shared = Arc::new(RwLock::new(default_tool_policy()));
        let policy = AgentPolicy::new(shared.clone());

        // Default: bash_background_read is allow.
        assert!(matches!(
            policy.evaluate(names::BG_READ, "{}").unwrap(),
            ToolDecision::Allow
        ));

        // Update policy: set bash_background_read to deny.
        {
            let mut p = shared.write().unwrap();
            p.tools.insert(names::BG_READ.into(), PolicyDecision::Deny);
        }

        // Now it should be denied.
        assert!(matches!(
            policy.evaluate(names::BG_READ, "{}").unwrap(),
            ToolDecision::Deny { .. }
        ));
    }
}
