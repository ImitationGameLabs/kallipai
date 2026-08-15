//! Authorized tool executor with approval gating.

use std::sync::Arc;

use anyhow::Result;
use just_llm_client::{ToolDispatcher, types::chat::ToolDefinition};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

use super::AgentPolicy;
use crate::approval::{ApprovalStatus, ApprovalStore, approval_result_json};
use crate::tools;

/// Outcome of a single tool call, for the runner's stop-on-non-success policy.
/// Each variant carries the result envelope (the same `String` that becomes the
/// `tool`-role message) unchanged; the variant only drives whether the rest of
/// the round is skipped.
///
/// `Success` = the call cleanly achieved its goal (`ok:true`, and for bash_exec
/// a foreground `exit_code` of 0; a `background:true` spawn counts as success).
/// `Failed` = tool-level error/deny/timeout, or a bash foreground non-zero/null
/// exit. `Deferred` = pending approval.
#[derive(Debug)]
pub enum ToolCallOutcome {
    Success(String),
    Failed(String),
    Deferred(String),
}

/// Executes tools behind a policy gate with approval gating.
///
/// When policy returns `Ask`, the tool call is stored in the
/// [`ApprovalStore`] and a pending-approval result is returned immediately.
/// The LLM can continue working. When approval arrives, the LLM
/// calls `approval_redeem` to execute the stored action.
/// Also delivers operator-declared post-call hook notes on the notice channel
/// (v1: every `bash_exec` call whose command ran — redeemed approvals too;
/// use is the trigger, exit code irrelevant).
pub struct AuthorizedToolExecutor {
    dispatch: ToolDispatcher,
    policy: AgentPolicy,
    approvals: Arc<Mutex<ApprovalStore>>,
    notice_sink: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

impl AuthorizedToolExecutor {
    pub fn new(
        dispatch: ToolDispatcher,
        policy: AgentPolicy,
        approvals: Arc<Mutex<ApprovalStore>>,
        notice_sink: Option<Arc<dyn Fn(String) + Send + Sync>>,
    ) -> Self {
        Self {
            dispatch,
            policy,
            approvals,
            notice_sink,
        }
    }

    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = self.dispatch.tool_definitions();
        defs.push(tools::break_definition());
        defs.push(tools::approval_list_definition());
        defs.push(tools::approval_commit_definition());
        defs.push(tools::approval_redeem_definition());
        defs.push(tools::approval_cancel_definition());
        defs
    }

    pub async fn execute(&mut self, tool_name: &str, args_json: &str) -> ToolCallOutcome {
        let envelope = match tool_name {
            "approval_list" => self.handle_list(args_json).await,
            "approval_commit" => self.handle_commit(args_json).await,
            "approval_redeem" => self.handle_redeem(args_json).await,
            "approval_cancel" => self.handle_cancel(args_json).await,
            _ => self.execute_tool(tool_name, args_json).await,
        };
        classify_outcome(envelope)
    }

    async fn execute_tool(&mut self, tool_name: &str, args_json: &str) -> String {
        let decision = match self.policy.evaluate(tool_name, args_json) {
            Ok(d) => d,
            Err(e) => return error_result(tool_name, format!("policy evaluation failed: {e:#}")),
        };

        match decision {
            super::ToolDecision::Allow => match self.dispatch.call_tool(tool_name, args_json).await
            {
                Ok(output) => {
                    // Use is the trigger: bash ran the command, so the
                    // note fires whatever exit code it recorded.
                    self.fire_hook_notes(tool_name, args_json);
                    success_result(tool_name, output)
                }
                Err(e) => error_result(tool_name, e.to_string()),
            },
            super::ToolDecision::Deny { reason } => {
                error_result(tool_name, format!("tool denied: {reason}"))
            }
            super::ToolDecision::Ask { reason } => {
                let mut q = self.approvals.lock().await;
                let id = q.enqueue(tool_name, args_json, reason.clone());
                approval_result_json(&id, tool_name, reason.as_deref())
            }
        }
    }

    /// Deliver post-call hook notes once a `tool_name` call's command ran.
    /// Use is the trigger — the note fires whatever exit code came back,
    /// while calls that never reached dispatch (denied, deferred, or
    /// infrastructure errors) fire nothing.
    ///
    /// Notes ride the notice channel (the same pipeline as background-task
    /// notices) as independent `[hook] NOTE:` messages — never the tool
    /// result itself. A full channel drops silently (`try_send` upstream):
    /// a nudge, not correctness.
    /// A background spawn (`background:true`, or a timeout conversion) is a
    /// successful dispatch, not a completed command — its note fires at
    /// spawn time, before the outcome is knowable. Bounded by the notes'
    /// self-conditional wording; v1 accepts spawn time as the trigger point.
    fn fire_hook_notes(&self, tool_name: &str, args_json: &str) {
        let Some(sink) = &self.notice_sink else {
            return;
        };
        for note in self.policy.post_hook_notes(tool_name, args_json) {
            sink(format!("[hook] NOTE: {note}"));
        }
    }

    async fn handle_list(&self, args_json: &str) -> String {
        let status_filter = parse_status_filter(args_json);
        let q = self.approvals.lock().await;
        let items: Vec<_> = q
            .list(status_filter.as_ref())
            .into_iter()
            .map(|a| ApprovalListItem {
                id: a.id,
                content: a.content,
                commit_reason: a.commit_reason,
                status: a.status,
                deny_reason: a.deny_reason,
                defer_reason: a.defer_reason,
                created_at: a.created_at,
            })
            .collect();
        serde_json::to_string(&ApprovalListResponse {
            ok: true,
            actions: items,
        })
        .unwrap_or_else(|e| error_result("approval_list", e.to_string()))
    }

    async fn handle_commit(&self, args_json: &str) -> String {
        let (id, reason) = match parse_commit_args(args_json) {
            Ok(v) => v,
            Err(e) => return error_result("approval_commit", e.to_string()),
        };
        let mut q = self.approvals.lock().await;
        match q.commit(&id, &reason) {
            Ok(()) => serde_json::to_string(&ApprovalCommitResponse {
                ok: true,
                committed: true,
                id,
            })
            .unwrap_or_else(|e| error_result("approval_commit", e.to_string())),
            Err(e) => error_result("approval_commit", e.to_string()),
        }
    }

    async fn handle_redeem(&mut self, args_json: &str) -> String {
        let id = match parse_id(args_json) {
            Ok(id) => id,
            Err(e) => return error_result("approval_redeem", e.to_string()),
        };
        let action = {
            let mut q = self.approvals.lock().await;
            match q.take_for_redeem(&id) {
                Ok(a) => a,
                Err(e) => return error_result("approval_redeem", e.to_string()),
            }
        };
        match self
            .dispatch
            .call_tool(&action.tool_name, &action.args_json)
            .await
        {
            Ok(output) => {
                // The stored action is the real (approved) tool call — hook
                // notes must observe it here, even though `execute` only saw
                // the redeem tool's name. Use is the trigger, same as the
                // direct path: the re-run executed, so its note fires
                // regardless of the inner exit code.
                self.fire_hook_notes(&action.tool_name, &action.args_json);
                success_result(&action.tool_name, output)
            }
            Err(e) => error_result(&action.tool_name, e.to_string()),
        }
    }

    async fn handle_cancel(&self, args_json: &str) -> String {
        let id = match parse_id(args_json) {
            Ok(id) => id,
            Err(e) => return error_result("approval_cancel", e.to_string()),
        };
        let mut q = self.approvals.lock().await;
        match q.cancel(&id) {
            Ok(prev) => serde_json::to_string(&ApprovalCancelResponse {
                ok: true,
                cancelled: id,
                previous_status: prev.to_string(),
            })
            .unwrap_or_else(|e| error_result("approval_cancel", e.to_string())),
            Err(e) => error_result("approval_cancel", e.to_string()),
        }
    }
}

fn parse_id(args_json: &str) -> Result<String> {
    let v: Value = serde_json::from_str(args_json)?;
    v.get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid 'id' field"))
}

fn parse_status_filter(args_json: &str) -> Option<ApprovalStatus> {
    let v: Value = serde_json::from_str(args_json).ok()?;
    let s = v.get("status")?.as_str()?;
    ApprovalStatus::from_str_name(s)
}

fn parse_commit_args(args_json: &str) -> Result<(String, String)> {
    let v: Value = serde_json::from_str(args_json)?;
    let id = v
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid 'id' field"))?
        .to_owned();
    let reason = v
        .get("reason")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid 'reason' field"))?
        .to_owned();
    Ok((id, reason))
}

fn success_result(tool_name: &str, output: String) -> String {
    let parsed = serde_json::from_str::<Value>(&output).unwrap_or(Value::String(output));
    serde_json::to_string(&ToolResultResponse {
        ok: true,
        tool_name: tool_name.to_owned(),
        result: parsed,
    })
    .unwrap_or_else(|e| error_result(tool_name, e.to_string()))
}

pub(crate) fn error_result(tool_name: &str, error: String) -> String {
    serde_json::to_string(&ToolErrorResponse {
        ok: false,
        tool_name: tool_name.to_owned(),
        error,
    })
    .unwrap_or_else(|_| r#"{"ok":false,"error":"serialization failed"}"#.to_owned())
}

/// Synthetic error envelope for a tool call skipped because an earlier call in
/// the same round did not cleanly succeed. The runner emits this (instead of
/// executing) so every `tool_use` still gets a result.
pub(crate) fn skipped_tool_result(tool_name: &str, prior_name: &str, reason: &str) -> String {
    error_result(
        tool_name,
        format!("skipped: earlier tool call '{prior_name}' {reason} in this round"),
    )
}

/// Error envelope for a tool call that exceeded its timeout. Normalizes what
/// used to be a bare diagnostic string into the standard `ok:false` envelope.
pub(crate) fn timed_out_tool_result(tool_name: &str, secs: u64) -> String {
    error_result(tool_name, format!("timed out after {secs}s"))
}

/// Classify a result envelope into a [`ToolCallOutcome`]. The runner uses the
/// variant to decide stop-and-skip; the envelope string itself is returned to
/// the agent unchanged. All tool-specific knowledge (the bash foreground
/// exit-code rule, the background `task_id` carve-out) lives here, not in the
/// runner.
fn classify_outcome(envelope: String) -> ToolCallOutcome {
    let Ok(v) = serde_json::from_str::<Value>(&envelope) else {
        // Unparseable envelope (should not happen): don't second-guess success.
        return ToolCallOutcome::Success(envelope);
    };
    if v.get("pending_approval").and_then(|b| b.as_bool()) == Some(true) {
        return ToolCallOutcome::Deferred(envelope);
    }
    if v.get("ok").and_then(|b| b.as_bool()) == Some(false) {
        return ToolCallOutcome::Failed(envelope);
    }
    // ok:true. A bash_exec foreground result is only a clean success at exit 0;
    // background spawns (task_id present) succeed regardless of exit_code.
    // Note: approval_redeem re-runs the stored call under its own tool_name, so
    // this also classifies a redeemed bash_exec by its inner exit code.
    if v.get("tool_name").and_then(|s| s.as_str()) == Some("bash_exec")
        && bash_foreground_failed(&v)
    {
        return ToolCallOutcome::Failed(envelope);
    }
    ToolCallOutcome::Success(envelope)
}

/// `true` iff this is a foreground bash_exec result that did not exit 0
/// (non-zero exit, or null exit_code = signal death). Background spawns
/// (`task_id` present) are NOT foreground failures.
fn bash_foreground_failed(v: &Value) -> bool {
    let Some(result) = v.get("result") else {
        return false;
    };
    if result.get("task_id").and_then(|t| t.as_str()).is_some() {
        return false; // background spawn
    }
    // exit_code == 0 -> clean; anything else (non-zero, or null = signal death)
    // is a foreground failure.
    result.get("exit_code").and_then(|c| c.as_i64()) != Some(0)
}

// -- Typed response structs --

#[derive(Serialize)]
struct ToolResultResponse {
    ok: bool,
    tool_name: String,
    result: Value,
}

#[derive(Serialize)]
struct ToolErrorResponse {
    ok: bool,
    tool_name: String,
    error: String,
}

#[derive(Serialize)]
struct ApprovalListResponse {
    ok: bool,
    actions: Vec<ApprovalListItem>,
}

#[derive(Serialize)]
struct ApprovalListItem {
    id: String,
    content: kallip_common::approval::ToolCallContent,
    commit_reason: Option<String>,
    status: ApprovalStatus,
    deny_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    defer_reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: time::OffsetDateTime,
}

#[derive(Serialize)]
struct ApprovalCommitResponse {
    ok: bool,
    committed: bool,
    id: String,
}

#[derive(Serialize)]
struct ApprovalCancelResponse {
    ok: bool,
    cancelled: String,
    previous_status: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Trigger;
    use kallip_shell::BashExecOutput;

    /// Build a bash_exec tool-output JSON string (the shape `call_tool` returns),
    /// mirroring the relevant `BashExecOutput` fields.
    fn bash_output(exit_code: Option<i32>, task_id: Option<&str>) -> String {
        let exit = match exit_code {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        let task = match task_id {
            Some(t) => format!(",\"task_id\":\"{t}\""),
            None => String::new(),
        };
        format!(
            "{{\"output\":\"o\",\"exit_code\":{exit},\"timed_out\":false,\"truncated\":false,\"cwd\":\"/tmp\"{task}}}"
        )
    }

    #[test]
    fn classify_bash_foreground_zero_is_success() {
        let env = success_result("bash_exec", bash_output(Some(0), None));
        assert!(matches!(classify_outcome(env), ToolCallOutcome::Success(_)));
    }

    #[test]
    fn classify_bash_foreground_nonzero_is_failed() {
        let env = success_result("bash_exec", bash_output(Some(2), None));
        assert!(matches!(classify_outcome(env), ToolCallOutcome::Failed(_)));
    }

    #[test]
    fn classify_bash_signal_death_null_exit_is_failed() {
        let env = success_result("bash_exec", bash_output(None, None));
        assert!(matches!(classify_outcome(env), ToolCallOutcome::Failed(_)));
    }

    #[test]
    fn classify_bash_timeout_conversion_is_success() {
        // The REAL serialized envelope of a timed-out exec converted to a
        // still-running background task (timed_out:true + task_id +
        // exit_code null): a success — the task_id carve-out must keep
        // classifying it as non-failure despite the null exit code.
        let out = BashExecOutput {
            output: Some("partial".into()),
            stdout: None,
            stderr: None,
            exit_code: None,
            timed_out: true,
            truncated: false,
            cwd: "/tmp".into(),
            task_id: Some("t9".into()),
        };
        let env = success_result("bash_exec", serde_json::to_string(&out).unwrap());
        assert!(matches!(classify_outcome(env), ToolCallOutcome::Success(_)));
    }

    #[test]
    fn classify_bash_refused_conversion_is_failed() {
        // The refused-conversion envelope (a carve-out landed, so the child
        // was killed under the old timeout semantics): timed_out:true but
        // NO task_id and a null exit code — the successor of the old
        // exit-124-is-failed pin.
        let out = BashExecOutput {
            output: Some("[timed out; killed ...]".into()),
            stdout: None,
            stderr: None,
            exit_code: None,
            timed_out: true,
            truncated: false,
            cwd: "/tmp".into(),
            task_id: None,
        };
        let env = success_result("bash_exec", serde_json::to_string(&out).unwrap());
        assert!(matches!(classify_outcome(env), ToolCallOutcome::Failed(_)));
    }

    #[test]
    fn classify_bash_background_spawn_is_success() {
        // background:true returns task_id + exit_code null. It is a success
        // (process spawned) and must NOT be misclassified as a foreground
        // failure despite the null exit_code.
        let env = success_result("bash_exec", bash_output(None, Some("t1")));
        assert!(matches!(classify_outcome(env), ToolCallOutcome::Success(_)));
    }

    #[test]
    fn classify_non_bash_success_is_success() {
        let env = success_result("context_pin", "{\"label\":\"x\"}".to_string());
        assert!(matches!(classify_outcome(env), ToolCallOutcome::Success(_)));
    }

    #[test]
    fn classify_ok_false_is_failed() {
        let env = error_result("bash_exec", "tool denied: destructive".to_string());
        assert!(matches!(classify_outcome(env), ToolCallOutcome::Failed(_)));
    }

    #[test]
    fn classify_pending_approval_is_deferred() {
        let env = approval_result_json("ap_x", "bash_exec", None);
        assert!(matches!(
            classify_outcome(env),
            ToolCallOutcome::Deferred(_)
        ));
    }

    #[test]
    fn classify_redeemed_bash_nonzero_is_failed() {
        // approval_redeem re-runs the stored call; its envelope carries the
        // inner tool_name (bash_exec) + exit code, so the same rule applies.
        let env = success_result("bash_exec", bash_output(Some(2), None));
        assert!(matches!(classify_outcome(env), ToolCallOutcome::Failed(_)));
    }

    #[test]
    fn skipped_and_timed_out_envelopes_are_ok_false() {
        let s = skipped_tool_result("rm", "bash_exec", "did not succeed");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["tool_name"], "rm");
        assert!(v["error"].as_str().unwrap().contains("skipped"));
        assert!(v["error"].as_str().unwrap().contains("bash_exec"));

        let t = timed_out_tool_result("bash_exec", 30);
        let v: Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("30s"));
    }

    // -- Post-call hook notes (mock shell backend, captured notice sink) --

    use std::sync::Mutex as StdMutex;

    use crate::policy::HookPhase;
    use crate::policy::HookRule;
    use kallip_shell::MockShellBackend;
    use kallip_shell::tools::names;
    use tokio::sync::Mutex as TokioMutex;

    fn hook_rule(prefix: &[&str], note: &str) -> HookRule {
        HookRule {
            tool: names::BASH_EXEC.into(),
            trigger: Trigger::Prefix(prefix.iter().map(|p| (*p).into()).collect()),
            phase: HookPhase::Post,
            note: note.into(),
        }
    }

    fn make_hook_executor(
        rules: Vec<HookRule>,
        sink: Option<Arc<dyn Fn(String) + Send + Sync>>,
    ) -> (AuthorizedToolExecutor, Arc<TokioMutex<MockShellBackend>>) {
        let (tools, backend) = kallip_shell::mock_shell_tool_set();
        let mut dispatch = ToolDispatcher::new();
        dispatch.add_tools(tools).unwrap();
        let policy = AgentPolicy::new(
            Arc::new(std::sync::RwLock::new(
                kallip_common::policy::ExecPolicy::default(),
            )),
            kallip_common::policy::PolicyPreset::Default,
            Arc::new(rules),
        );
        let executor = AuthorizedToolExecutor::new(
            dispatch,
            policy,
            Arc::new(TokioMutex::new(ApprovalStore::new())),
            sink,
        );
        (executor, backend)
    }

    async fn queue_success(backend: &Arc<TokioMutex<MockShellBackend>>) {
        backend.lock().await.push_output("").push_exit_code(Some(0));
    }

    async fn queue_failure(backend: &Arc<TokioMutex<MockShellBackend>>) {
        backend.lock().await.push_output("").push_exit_code(Some(1));
    }

    #[tokio::test]
    async fn hook_note_fires_on_successful_matching_call() {
        let notes: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink_notes = notes.clone();
        let (mut executor, backend) = make_hook_executor(
            vec![hook_rule(&["echo", "hi"], "echo skill applies")],
            Some(Arc::new(move |text| sink_notes.lock().unwrap().push(text))),
        );
        queue_success(&backend).await;
        let outcome = executor
            .execute(names::BASH_EXEC, r#"{"command":"echo hi"}"#)
            .await;
        assert!(matches!(outcome, ToolCallOutcome::Success(_)));
        assert_eq!(
            notes.lock().unwrap().as_slice(),
            ["[hook] NOTE: echo skill applies"]
        );
    }

    #[tokio::test]
    async fn hook_note_not_fired_for_echoed_argument() {
        let notes: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink_notes = notes.clone();
        let (mut executor, backend) = make_hook_executor(
            vec![hook_rule(&["git", "commit"], "commit skill applies")],
            Some(Arc::new(move |text| sink_notes.lock().unwrap().push(text))),
        );
        queue_success(&backend).await;
        executor
            .execute(names::BASH_EXEC, r#"{"command":"echo \"git commit\""}"#)
            .await;
        assert!(notes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn hook_note_fires_on_nonzero_exit() {
        let notes: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink_notes = notes.clone();
        let (mut executor, backend) = make_hook_executor(
            vec![hook_rule(&["echo", "hi"], "echo skill applies")],
            Some(Arc::new(move |text| sink_notes.lock().unwrap().push(text))),
        );
        queue_failure(&backend).await;
        let outcome = executor
            .execute(names::BASH_EXEC, r#"{"command":"echo hi"}"#)
            .await;
        assert!(matches!(outcome, ToolCallOutcome::Failed(_)));
        // Use is the trigger: the command ran, exit 1 or not.
        assert_eq!(
            notes.lock().unwrap().as_slice(),
            ["[hook] NOTE: echo skill applies"]
        );
    }

    #[tokio::test]
    async fn empty_rules_mean_zero_behavior_change() {
        let notes: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink_notes = notes.clone();
        let (mut executor, backend) = make_hook_executor(
            vec![],
            Some(Arc::new(move |text| sink_notes.lock().unwrap().push(text))),
        );
        queue_success(&backend).await;
        let outcome = executor
            .execute(names::BASH_EXEC, r#"{"command":"echo hi"}"#)
            .await;
        assert!(matches!(outcome, ToolCallOutcome::Success(_)));
        assert!(notes.lock().unwrap().is_empty());
        assert_eq!(backend.lock().await.executed_commands(), ["echo hi"]);
    }

    #[tokio::test]
    async fn missing_sink_never_panics() {
        let (mut executor, backend) =
            make_hook_executor(vec![hook_rule(&["echo", "hi"], "echo skill applies")], None);
        queue_success(&backend).await;
        let outcome = executor
            .execute(names::BASH_EXEC, r#"{"command":"echo hi"}"#)
            .await;
        assert!(matches!(outcome, ToolCallOutcome::Success(_)));
    }

    /// The flagship path under the default preset: `git commit` is Ask-gated,
    /// so the note can only fire when the redeemed action executes.
    #[tokio::test]
    async fn redeemed_ask_gated_call_fires_hook_note() {
        let notes: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink_notes = notes.clone();
        let (mut executor, backend) = make_hook_executor(
            vec![hook_rule(&["git", "commit"], "commit skill applies")],
            Some(Arc::new(move |text| sink_notes.lock().unwrap().push(text))),
        );
        // 1. Ask-gated call defers with a pending-approval id.
        let deferred = match executor
            .execute(names::BASH_EXEC, r#"{"command":"git commit -m x"}"#)
            .await
        {
            ToolCallOutcome::Deferred(envelope) => envelope,
            other => panic!("expected Deferred, got {other:?}"),
        };
        assert!(notes.lock().unwrap().is_empty());
        let id = serde_json::from_str::<Value>(&deferred).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        // 2. Operator-side approval lifecycle, then redeem.
        {
            let mut q = executor.approvals.lock().await;
            q.commit(&id, "test").unwrap();
            q.approve(&id).unwrap();
        }
        queue_success(&backend).await;
        let outcome = executor
            .execute("approval_redeem", &format!("{{\"id\":\"{id}\"}}"))
            .await;
        assert!(matches!(outcome, ToolCallOutcome::Success(_)));
        assert_eq!(
            notes.lock().unwrap().as_slice(),
            ["[hook] NOTE: commit skill applies"]
        );
        assert_eq!(
            backend.lock().await.executed_commands(),
            ["git commit -m x"]
        );
    }

    /// A redeemed re-run that exits non-zero still ran the command —
    /// use is the trigger, so the note fires (outcome aside).
    #[tokio::test]
    async fn redeemed_failed_call_still_fires_hook_note() {
        let notes: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink_notes = notes.clone();
        let (mut executor, backend) = make_hook_executor(
            vec![hook_rule(&["git", "commit"], "commit skill applies")],
            Some(Arc::new(move |text| sink_notes.lock().unwrap().push(text))),
        );
        let deferred = match executor
            .execute(names::BASH_EXEC, r#"{"command":"git commit -m x"}"#)
            .await
        {
            ToolCallOutcome::Deferred(envelope) => envelope,
            other => panic!("expected Deferred, got {other:?}"),
        };
        let id = serde_json::from_str::<Value>(&deferred).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        {
            let mut q = executor.approvals.lock().await;
            q.commit(&id, "test").unwrap();
            q.approve(&id).unwrap();
        }
        // The re-run exits non-zero (pre-commit hook rejection, say).
        queue_failure(&backend).await;
        let outcome = executor
            .execute("approval_redeem", &format!("{{\"id\":\"{id}\"}}"))
            .await;
        assert!(matches!(outcome, ToolCallOutcome::Failed(_)));
        assert_eq!(
            notes.lock().unwrap().as_slice(),
            ["[hook] NOTE: commit skill applies"]
        );
    }
}
