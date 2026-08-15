//! Agent policy: gates `bash_exec` via a preset-aware classifier; every other
//! tool runs unconditionally (it is the agent's own self-management).

use std::sync::{Arc, RwLock};

use anyhow::Result;
use kallip_common::policy::{ExecPolicy, PolicyPreset};

use kallip_shell::tools::{BashExecArgs, names};

use super::ToolDecision;
use super::classifier::Classifier;
use super::classifier::hooks::{HookRule, hook_matches, scan_command};

/// Policy layer that gates tool calls.
///
/// Only `bash_exec` is gated (it is the arbitrary-execution surface); every other
/// tool is unconditionally `Allow`. The `bash_exec` verdict comes from a
/// preset-aware [`Classifier`] applied to a snapshot of the shared per-agent
/// [`ExecPolicy`] overrides. The preset is fixed for the agent's lifetime
/// (tagma-global, selected once at startup), while the exec-policy is
/// runtime-mutable.
/// It also owns the operator-declared hook rules (post-call compliance
/// notes; v1 observes `bash_exec` only) — fixed for the agent's lifetime.
#[derive(Clone, Debug)]
pub struct AgentPolicy {
    exec_policy: Arc<RwLock<ExecPolicy>>,
    classifier: Classifier,
    preset: PolicyPreset,
    hook_rules: Arc<Vec<HookRule>>,
}

impl AgentPolicy {
    pub fn new(
        exec_policy: Arc<RwLock<ExecPolicy>>,
        preset: PolicyPreset,
        hook_rules: Arc<Vec<HookRule>>,
    ) -> Self {
        Self {
            exec_policy,
            classifier: Classifier::DEFAULT,
            preset,
            hook_rules,
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

    /// Post-call hook notes for a `tool_name` invocation whose command ran.
    ///
    /// v1 observes `bash_exec` only, and callers invoke this exclusively
    /// once the command has been dispatched (the design's post phase): use
    /// is the trigger, so a non-zero exit still notes. Deny/Ask paths —
    /// the command never ran — never produce notes. Empty rules
    /// short-circuit before any parsing — no rules configured means zero
    /// behavior change.
    pub fn post_hook_notes(&self, tool_name: &str, args_json: &str) -> Vec<String> {
        if self.hook_rules.is_empty() || tool_name != names::BASH_EXEC {
            return Vec::new();
        }
        let Ok(args) = serde_json::from_str::<BashExecArgs>(args_json) else {
            return Vec::new();
        };
        let (segments, write_redirect) = scan_command(&args.command);
        hook_matches(&self.hook_rules, &segments, write_redirect)
            .into_iter()
            .map(|rule| rule.note.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Trigger;
    use crate::policy::classifier::hooks::HookPhase;
    use kallip_common::policy::{ExecDecision, ExecOverride, PolicyPreset};

    fn make_policy(preset: PolicyPreset) -> AgentPolicy {
        AgentPolicy::new(
            Arc::new(RwLock::new(ExecPolicy::default())),
            preset,
            Arc::new(Vec::new()),
        )
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
        let policy = AgentPolicy::new(exec.clone(), PolicyPreset::Default, Arc::new(Vec::new()));
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

    fn git_commit_rule() -> HookRule {
        HookRule {
            tool: names::BASH_EXEC.into(),
            trigger: Trigger::Prefix(vec!["git".into(), "commit".into()]),
            phase: HookPhase::Post,
            note: "a commit message was just written".into(),
        }
    }

    fn hook_policy(preset: PolicyPreset, rules: Vec<HookRule>) -> AgentPolicy {
        AgentPolicy::new(
            Arc::new(RwLock::new(ExecPolicy::default())),
            preset,
            Arc::new(rules),
        )
    }

    #[test]
    fn post_hook_notes_fire_on_matching_bash_command() {
        let policy = hook_policy(PolicyPreset::Auto, vec![git_commit_rule()]);
        let notes = policy.post_hook_notes(names::BASH_EXEC, r#"{"command":"git commit -m x"}"#);
        assert_eq!(notes, ["a commit message was just written"]);
    }

    #[test]
    fn post_hook_notes_empty_rules_or_non_bash_tool_give_nothing() {
        // Empty rules: zero behavior change, no parsing at all.
        let policy = hook_policy(PolicyPreset::Auto, vec![]);
        assert!(
            policy
                .post_hook_notes(names::BASH_EXEC, r#"{"command":"git commit"}"#)
                .is_empty()
        );
        // Non-bash_exec tools are never observed.
        let policy = hook_policy(PolicyPreset::Auto, vec![git_commit_rule()]);
        assert!(policy.post_hook_notes(names::BG_READ, "{}").is_empty());
    }

    #[test]
    fn post_hook_notes_survive_malformed_args() {
        let policy = hook_policy(PolicyPreset::Auto, vec![git_commit_rule()]);
        assert!(
            policy
                .post_hook_notes(names::BASH_EXEC, "not json")
                .is_empty()
        );
    }
}
