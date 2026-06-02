//! Deferred action store for async approval of tool calls.

use std::collections::{HashMap, VecDeque};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub use just_agent_common::types::{DeferredActionStatus, ToolCallContent};
use time::OffsetDateTime;

/// A tool action that was deferred pending approval.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeferredAction {
    pub id: String,
    pub tool_name: String,
    pub args_json: String,
    pub reason: String,
    pub dangerous: bool,
    pub status: DeferredActionStatus,
    pub deny_reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A lightweight snapshot returned by list operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeferredActionInfo {
    pub id: String,
    pub content: ToolCallContent,
    pub reason: String,
    pub dangerous: bool,
    pub status: DeferredActionStatus,
    pub deny_reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Info about the most recently committed action (consumed once).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeferredCommittedInfo {
    pub id: String,
    pub tool_name: String,
    pub args_json: String,
    pub reason: String,
    pub dangerous: bool,
}

/// Notification pushed when an external approval/denial arrives.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DeferredNotification {
    Approved { id: String },
    Denied { id: String, reason: String },
}

/// Store of deferred tool actions awaiting approval.
///
/// Shared between the executor (enqueue, redeem, cancel) and the daemon
/// routes (approve, deny). The runner drains notifications to inject into
/// the LLM context at the start of each round.
#[derive(Serialize, Deserialize)]
pub struct DeferredActionStore {
    actions: HashMap<String, DeferredAction>,
    /// Transient: drained each round, not persisted.
    #[serde(skip)]
    notifications: VecDeque<DeferredNotification>,
    /// Transient: one-shot flag for committed actions, not persisted.
    #[serde(skip)]
    last_committed: Option<DeferredCommittedInfo>,
    /// Transient: one-shot flag for redeemed actions, not persisted.
    #[serde(skip)]
    last_redeemed: Option<String>,
    /// Transient: one-shot flag for cancelled actions, not persisted.
    #[serde(skip)]
    last_cancelled: Option<String>,
}

impl Default for DeferredActionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DeferredActionStore {
    pub fn new() -> Self {
        Self {
            actions: HashMap::new(),
            notifications: VecDeque::new(),
            last_committed: None,
            last_redeemed: None,
            last_cancelled: None,
        }
    }

    /// Enqueue a deferred action and return the id.
    pub fn enqueue(
        &mut self,
        tool_name: &str,
        args_json: &str,
        reason: &str,
        dangerous: bool,
    ) -> String {
        // "da" prefix is short for "deferred action"
        let id = format!("da_{}", uuid::Uuid::new_v4().simple());
        let created_at = OffsetDateTime::now_utc();
        let action = DeferredAction {
            id: id.clone(),
            tool_name: tool_name.to_owned(),
            args_json: args_json.to_owned(),
            reason: reason.to_owned(),
            dangerous,
            status: DeferredActionStatus::Pending,
            deny_reason: None,
            created_at,
        };
        self.actions.insert(id.clone(), action);
        id
    }

    /// Approve a committed action.
    pub fn approve(&mut self, id: &str) -> Result<()> {
        let action = self
            .actions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("deferred action '{id}' not found"))?;
        if !matches!(action.status, DeferredActionStatus::Committed) {
            bail!(
                "deferred action '{id}' is not committed (status: {:?})",
                action.status
            );
        }
        action.status = DeferredActionStatus::Approved;
        self.notifications
            .push_back(DeferredNotification::Approved { id: id.to_owned() });
        Ok(())
    }

    /// Deny a committed action.
    pub fn deny(&mut self, id: &str, reason: &str) -> Result<()> {
        let action = self
            .actions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("deferred action '{id}' not found"))?;
        if !matches!(action.status, DeferredActionStatus::Committed) {
            bail!(
                "deferred action '{id}' is not committed (status: {:?})",
                action.status
            );
        }
        action.status = DeferredActionStatus::Denied;
        action.deny_reason = Some(reason.to_owned());
        self.notifications.push_back(DeferredNotification::Denied {
            id: id.to_owned(),
            reason: reason.to_owned(),
        });
        Ok(())
    }

    /// Take an approved action for redemption (marks it as redeemed).
    pub fn take_for_redeem(&mut self, id: &str) -> Result<DeferredAction> {
        let action = self
            .actions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("deferred action '{id}' not found"))?;
        match &action.status {
            DeferredActionStatus::Pending => {
                bail!("deferred action '{id}' is still pending — commit it first")
            }
            DeferredActionStatus::Committed => {
                bail!("deferred action '{id}' is awaiting approval")
            }
            DeferredActionStatus::Denied => {
                let reason = action.deny_reason.as_deref().unwrap_or("unknown");
                bail!("deferred action '{id}' was denied: {reason}")
            }
            DeferredActionStatus::Redeemed => {
                bail!("deferred action '{id}' has already been redeemed")
            }
            DeferredActionStatus::Cancelled => {
                bail!("deferred action '{id}' was cancelled")
            }
            DeferredActionStatus::Approved => {}
        }
        action.status = DeferredActionStatus::Redeemed;
        self.last_redeemed = Some(id.to_owned());
        Ok(action.clone())
    }

    /// Cancel an action that is no longer needed.
    ///
    /// Works on any non-terminal state (Pending, Committed, Approved, Denied).
    /// Returns the previous status so the caller can include it in the response.
    pub fn cancel(&mut self, id: &str) -> Result<DeferredActionStatus> {
        let action = self
            .actions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("deferred action '{id}' not found"))?;
        match action.status {
            DeferredActionStatus::Redeemed => {
                bail!("deferred action '{id}' has already been redeemed")
            }
            DeferredActionStatus::Cancelled => {
                bail!("deferred action '{id}' has already been cancelled")
            }
            prev => {
                action.status = DeferredActionStatus::Cancelled;
                self.last_cancelled = Some(id.to_owned());
                Ok(prev)
            }
        }
    }

    /// Commit a pending action with the agent's justification.
    ///
    /// Overwrites the classifier reason with the agent-provided one and
    /// transitions to `Committed` so the action becomes visible to superiors.
    pub fn commit(&mut self, id: &str, reason: &str) -> Result<()> {
        let action = self
            .actions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("deferred action '{id}' not found"))?;
        if !matches!(action.status, DeferredActionStatus::Pending) {
            bail!(
                "deferred action '{id}' is not pending (status: {:?})",
                action.status
            );
        }
        action.reason = reason.to_owned();
        action.status = DeferredActionStatus::Committed;
        self.last_committed = Some(DeferredCommittedInfo {
            id: id.to_owned(),
            tool_name: action.tool_name.clone(),
            args_json: action.args_json.clone(),
            reason: reason.to_owned(),
            dangerous: action.dangerous,
        });
        Ok(())
    }

    /// Take the info about the most recently committed action (one-shot).
    pub fn take_last_committed(&mut self) -> Option<DeferredCommittedInfo> {
        self.last_committed.take()
    }

    /// Take the ID of the most recently redeemed action (one-shot).
    pub fn take_last_redeemed(&mut self) -> Option<String> {
        self.last_redeemed.take()
    }

    /// Take the ID of the most recently cancelled action (one-shot).
    pub fn take_last_cancelled(&mut self) -> Option<String> {
        self.last_cancelled.take()
    }

    /// List deferred actions, optionally filtered by status.
    pub fn list(&self, status_filter: Option<&DeferredActionStatus>) -> Vec<DeferredActionInfo> {
        self.actions
            .values()
            .filter(|a| status_filter.is_none_or(|f| &a.status == f))
            .map(|a| DeferredActionInfo {
                id: a.id.clone(),
                content: ToolCallContent {
                    tool_name: a.tool_name.clone(),
                    arguments: serde_json::from_str(&a.args_json)
                        .unwrap_or(serde_json::Value::Null),
                },
                reason: a.reason.clone(),
                dangerous: a.dangerous,
                status: a.status,
                deny_reason: a.deny_reason.clone(),
                created_at: a.created_at,
            })
            .collect()
    }

    /// Check if the queue contains an action with the given id.
    pub fn contains(&self, id: &str) -> bool {
        self.actions.contains_key(id)
    }

    /// Look up a single action by id.
    pub fn get(&self, id: &str) -> Option<DeferredActionInfo> {
        self.actions.get(id).map(|a| DeferredActionInfo {
            id: a.id.clone(),
            content: ToolCallContent {
                tool_name: a.tool_name.clone(),
                arguments: serde_json::from_str(&a.args_json).unwrap_or(serde_json::Value::Null),
            },
            reason: a.reason.clone(),
            dangerous: a.dangerous,
            status: a.status,
            deny_reason: a.deny_reason.clone(),
            created_at: a.created_at,
        })
    }

    /// Drain all pending notifications (for runner context injection).
    pub fn drain_notifications(&mut self) -> Vec<DeferredNotification> {
        self.notifications.drain(..).collect()
    }
}

/// Format a deferred tool result JSON returned to the LLM.
pub fn deferred_result_json(id: &str, tool_name: &str, reason: &str, dangerous: bool) -> String {
    serde_json::json!({
        "ok": true,
        "deferred": true,
        "tool_name": tool_name,
        "id": id,
        "reason": reason,
        "dangerous": dangerous,
        "next_steps": "This tool call requires approval. \
            Call deferred_action_commit with the id and a justification for why it is necessary, \
            or deferred_action_cancel to abandon it."
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_commit_approve_redeem() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue(
            "shell_session_exec",
            r#"{"command":"rm -rf /tmp"}"#,
            "destructive",
            true,
        );
        assert!(id.starts_with("da_"));

        let info = q.list(None);
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].status, DeferredActionStatus::Pending);
        assert!(info[0].created_at <= OffsetDateTime::now_utc());

        q.commit(&id, "need to clean up temp dir").unwrap();
        assert_eq!(q.list(None)[0].status, DeferredActionStatus::Committed);
        assert_eq!(q.list(None)[0].reason, "need to clean up temp dir");

        q.approve(&id).unwrap();
        assert_eq!(q.list(None)[0].status, DeferredActionStatus::Approved);

        let action = q.take_for_redeem(&id).unwrap();
        assert_eq!(action.tool_name, "shell_session_exec");
        assert_eq!(action.reason, "need to clean up temp dir");
        assert_eq!(q.list(None)[0].status, DeferredActionStatus::Redeemed);
    }

    #[test]
    fn deny_prevents_redeem() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        q.commit(&id, "justification").unwrap();
        q.deny(&id, "no").unwrap();
        assert!(q.take_for_redeem(&id).is_err());
    }

    #[test]
    fn cancel_pending() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        let prev = q.cancel(&id).unwrap();
        assert_eq!(prev, DeferredActionStatus::Pending);
        assert_eq!(q.list(None)[0].status, DeferredActionStatus::Cancelled);
    }

    #[test]
    fn cancel_committed() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        q.commit(&id, "justification").unwrap();
        let prev = q.cancel(&id).unwrap();
        assert_eq!(prev, DeferredActionStatus::Committed);
        assert_eq!(q.list(None)[0].status, DeferredActionStatus::Cancelled);
    }

    #[test]
    fn cancel_approved() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        q.commit(&id, "justification").unwrap();
        q.approve(&id).unwrap();
        let prev = q.cancel(&id).unwrap();
        assert_eq!(prev, DeferredActionStatus::Approved);
        assert_eq!(q.list(None)[0].status, DeferredActionStatus::Cancelled);
    }

    #[test]
    fn cancel_denied() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        q.commit(&id, "justification").unwrap();
        q.deny(&id, "no").unwrap();
        let prev = q.cancel(&id).unwrap();
        assert_eq!(prev, DeferredActionStatus::Denied);
        assert_eq!(q.list(None)[0].status, DeferredActionStatus::Cancelled);
    }

    #[test]
    fn cannot_cancel_redeemed() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        q.commit(&id, "justification").unwrap();
        q.approve(&id).unwrap();
        q.take_for_redeem(&id).unwrap();
        assert!(q.cancel(&id).is_err());
    }

    #[test]
    fn cannot_cancel_already_cancelled() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        q.cancel(&id).unwrap();
        assert!(q.cancel(&id).is_err());
    }

    #[test]
    fn cannot_approve_non_committed() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        // Pending → cannot approve
        assert!(q.approve(&id).is_err());
        let _ = q.cancel(&id).unwrap();
        // Cancelled → cannot approve
        assert!(q.approve(&id).is_err());
    }

    #[test]
    fn cannot_redeem_pending_or_committed() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        assert!(q.take_for_redeem(&id).is_err());
        q.commit(&id, "justification").unwrap();
        assert!(q.take_for_redeem(&id).is_err());
    }

    #[test]
    fn commit_overwrites_reason() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "classifier reason", false);
        assert_eq!(q.list(None)[0].reason, "classifier reason");
        q.commit(&id, "agent justification").unwrap();
        assert_eq!(q.list(None)[0].reason, "agent justification");
    }

    #[test]
    fn cannot_commit_non_pending() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        q.commit(&id, "first").unwrap();
        // Already committed
        assert!(q.commit(&id, "second").is_err());
    }

    #[test]
    fn take_last_committed_is_one_shot() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        q.commit(&id, "justification").unwrap();
        let info = q.take_last_committed().unwrap();
        assert_eq!(info.id, id);
        assert_eq!(info.reason, "justification");
        assert!(q.take_last_committed().is_none());
    }

    #[test]
    fn drain_notifications() {
        let mut q = DeferredActionStore::new();
        let id1 = q.enqueue("t1", "{}", "r1", false);
        let id2 = q.enqueue("t2", "{}", "r2", true);
        q.commit(&id1, "j1").unwrap();
        q.commit(&id2, "j2").unwrap();
        q.approve(&id1).unwrap();
        q.deny(&id2, "no").unwrap();

        let notifs = q.drain_notifications();
        assert_eq!(notifs.len(), 2);
        assert!(q.drain_notifications().is_empty());
    }

    #[test]
    fn list_filters_by_status() {
        let mut q = DeferredActionStore::new();
        let id1 = q.enqueue("t1", "{}", "r", false);
        let id2 = q.enqueue("t2", "{}", "r", false);
        q.commit(&id1, "j1").unwrap();
        q.commit(&id2, "j2").unwrap();
        q.approve(&id1).unwrap();
        q.deny(&id2, "no").unwrap();

        let approved = q.list(Some(&DeferredActionStatus::Approved));
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].id, id1);

        let denied = q.list(Some(&DeferredActionStatus::Denied));
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].deny_reason.as_deref(), Some("no"));

        let pending = q.list(Some(&DeferredActionStatus::Pending));
        assert_eq!(pending.len(), 0);

        let committed = q.list(Some(&DeferredActionStatus::Committed));
        assert_eq!(committed.len(), 0);
    }

    #[test]
    fn not_found_errors() {
        let mut q = DeferredActionStore::new();
        assert!(q.approve("nonexistent").is_err());
        assert!(q.deny("nonexistent", "r").is_err());
        assert!(q.take_for_redeem("nonexistent").is_err());
        assert!(q.cancel("nonexistent").is_err());
        assert!(q.commit("nonexistent", "r").is_err());
    }

    #[test]
    fn contains_checks() {
        let mut q = DeferredActionStore::new();
        let id = q.enqueue("t", "{}", "reason", false);
        assert!(q.contains(&id));
        assert!(!q.contains("nonexistent"));
    }

    #[test]
    fn info_has_tool_call_content() {
        let mut q = DeferredActionStore::new();
        q.enqueue("shell_session_exec", r#"{"command":"ls"}"#, "reason", false);
        let info = &q.list(None)[0];
        assert_eq!(info.content.tool_name, "shell_session_exec");
        assert_eq!(info.content.arguments["command"], "ls");
    }
}
