//! Agent policy: gates `bash_exec` via a preset-aware classifier; every other
//! tool runs unconditionally (it is the agent's own self-management).

use std::sync::{Arc, RwLock};

use anyhow::Result;
use kallip_common::policy::{ExecPolicy, PolicyPreset};

use kallip_shell::tools::{BashExecArgs, names};

use super::ToolDecision;
use super::classifier::Classifier;

/// Policy layer that gates tool calls.
///
/// Only `bash_exec` is gated (it is the arbitrary-execution surface); every other
/// tool is unconditionally `Allow`. The `bash_exec` verdict comes from a
/// preset-aware [`Classifier`] applied to a snapshot of the shared per-agent
/// [`ExecPolicy`] overrides. The preset is fixed for the agent's lifetime
/// (daemon-global, selected once at startup), while the exec-policy is
/// runtime-mutable.
#[derive(Clone, Debug)]
pub struct AgentPolicy {
    exec_policy: Arc<RwLock<ExecPolicy>>,
    classifier: Classifier,
    preset: PolicyPreset,
}

impl AgentPolicy {
    pub fn new(exec_policy: Arc<RwLock<ExecPolicy>>, preset: PolicyPreset) -> Self {
        Self {
            exec_policy,
            classifier: Classifier::DEFAULT,
            preset,
        }
    }

    pub fn evaluate(&self, tool_name: &str, args_json: &str) -> Result<ToolDecision> {
        if tool_name == names::BASH_EXEC {
            return self.classify_bash(args_json);
        }
        // Every non-bash_exec tool is the agent's own self-management (context,
        // skills, background tasks, exec-policy query, approval redemption) with no
        // security surface — it runs unconditionally.
        Ok(ToolDecision::Allow)
    }

    /// Parse `bash_exec` args and classify the command under the agent's preset
    /// against a snapshot of the current exec-policy overrides.
    fn classify_bash(&self, args_json: &str) -> Result<ToolDecision> {
        let args: BashExecArgs = serde_json::from_str(args_json)?;
        let overrides = self
            .exec_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Ok(self
            .classifier
            .classify_with(&args.command, &overrides, self.preset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_common::policy::{ExecDecision, ExecOverride, PolicyPreset};

    fn make_policy(preset: PolicyPreset) -> AgentPolicy {
        AgentPolicy::new(Arc::new(RwLock::new(ExecPolicy::default())), preset)
    }

    #[test]
    fn non_bash_tool_allows_under_default() {
        let policy = make_policy(PolicyPreset::Default);
        assert!(matches!(
            policy.evaluate(names::BG_READ, "{}").unwrap(),
            ToolDecision::Allow
        ));
        assert!(matches!(
            policy.evaluate("some_new_tool", "{}").unwrap(),
            ToolDecision::Allow
        ));
    }

    #[test]
    fn non_bash_tool_allows_under_auto_and_allow_all() {
        for preset in [PolicyPreset::Auto, PolicyPreset::AllowAll] {
            let policy = make_policy(preset);
            assert!(
                matches!(
                    policy.evaluate(names::BG_READ, "{}").unwrap(),
                    ToolDecision::Allow
                ),
                "{preset:?}: bg_read should allow"
            );
            assert!(
                matches!(
                    policy.evaluate("some_new_tool", "{}").unwrap(),
                    ToolDecision::Allow
                ),
                "{preset:?}: unknown tool should allow"
            );
        }
    }

    #[test]
    fn bash_exec_returns_classifier_decision_under_default() {
        let policy = make_policy(PolicyPreset::Default);
        assert!(matches!(
            policy
                .evaluate(names::BASH_EXEC, r#"{"command":"ls"}"#)
                .unwrap(),
            ToolDecision::Allow
        ));
        assert!(matches!(
            policy
                .evaluate(names::BASH_EXEC, r#"{"command":"cargo"}"#)
                .unwrap(),
            ToolDecision::Ask { .. }
        ));
        assert!(matches!(
            policy
                .evaluate(names::BASH_EXEC, r#"{"command":"sed x"}"#)
                .unwrap(),
            ToolDecision::Deny { .. }
        ));
    }

    #[test]
    fn bash_exec_auto_allows_unclassified_keeps_denylist() {
        let policy = make_policy(PolicyPreset::Auto);
        assert!(matches!(
            policy
                .evaluate(names::BASH_EXEC, r#"{"command":"cargo"}"#)
                .unwrap(),
            ToolDecision::Allow
        ));
        assert!(matches!(
            policy
                .evaluate(names::BASH_EXEC, r#"{"command":"sed x"}"#)
                .unwrap(),
            ToolDecision::Deny { .. }
        ));
    }

    #[test]
    fn bash_exec_allow_all_bypasses_everything() {
        let policy = make_policy(PolicyPreset::AllowAll);
        assert_eq!(
            policy
                .evaluate(names::BASH_EXEC, r#"{"command":"sed x"}"#)
                .unwrap(),
            ToolDecision::Allow
        );
        assert_eq!(
            policy
                .evaluate(names::BASH_EXEC, r#"{"command":"rm -rf /"}"#)
                .unwrap(),
            ToolDecision::Allow
        );
    }

    #[test]
    fn exec_policy_override_widens_and_narrows() {
        let exec = Arc::new(RwLock::new(ExecPolicy::default()));

        // Widen an absent command (`cargo`) to Allow under the strict preset.
        exec.write()
            .unwrap()
            .overrides
            .insert("cargo".into(), ExecOverride::new(ExecDecision::Allow));
        let policy = AgentPolicy::new(exec.clone(), PolicyPreset::Default);
        assert!(matches!(
            policy
                .evaluate(names::BASH_EXEC, r#"{"command":"cargo"}"#)
                .unwrap(),
            ToolDecision::Allow
        ));

        // Narrow a catalog command (`ls`) to Deny with a surfaced reason.
        exec.write().unwrap().overrides.insert(
            "ls".into(),
            ExecOverride::new(ExecDecision::Deny).with_reason("no ls here"),
        );
        match policy
            .evaluate(names::BASH_EXEC, r#"{"command":"ls"}"#)
            .unwrap()
        {
            ToolDecision::Deny { reason } => assert_eq!(reason, "no ls here"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }
}
