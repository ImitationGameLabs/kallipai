//! Approval store for async approval of tool calls.

use std::collections::{HashMap, VecDeque};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub use just_agent_common::types::{ApprovalStatus, ToolCallContent};
use time::OffsetDateTime;

/// A tool action pending approval.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalAction {
    pub id: String,
    pub tool_name: String,
    pub args_json: String,
    /// Agent-provided justification set during commit.
    pub commit_reason: Option<String>,
    pub status: ApprovalStatus,
    pub deny_reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<&ApprovalAction> for ApprovalInfo {
    fn from(a: &ApprovalAction) -> Self {
        Self {
            id: a.id.clone(),
            content: ToolCallContent {
                tool_name: a.tool_name.clone(),
                arguments: serde_json::from_str(&a.args_json).unwrap_or(serde_json::Value::Null),
            },
            commit_reason: a.commit_reason.clone(),
            status: a.status,
            deny_reason: a.deny_reason.clone(),
            created_at: a.created_at,
        }
    }
}

/// A lightweight snapshot returned by list operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalInfo {
    pub id: String,
    pub content: ToolCallContent,
    /// Agent-provided justification (set during commit, empty for pending).
    pub commit_reason: Option<String>,
    pub status: ApprovalStatus,
    pub deny_reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Info about the most recently committed action (consumed once).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalCommittedInfo {
    pub id: String,
    pub tool_name: String,
    pub args_json: String,
    /// Agent-provided justification for why the tool call is necessary.
    pub commit_reason: String,
}

/// Notification pushed when an external approval/denial arrives.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ApprovalNotification {
    Approved { id: String },
    Denied { id: String, reason: String },
}

/// Store of approval requests awaiting review.
///
/// Shared between the executor (enqueue, redeem, cancel) and the daemon
/// routes (approve, deny). The runner drains notifications to inject into
/// the LLM context at the start of each round.
#[derive(Serialize, Deserialize)]
pub struct ApprovalStore {
    actions: HashMap<String, ApprovalAction>,
    /// Transient: drained each round, not persisted.
    #[serde(skip)]
    notifications: VecDeque<ApprovalNotification>,
    /// Transient: one-shot flag for committed actions, not persisted.
    #[serde(skip)]
    last_committed: Option<ApprovalCommittedInfo>,
    /// Transient: one-shot flag for redeemed actions, not persisted.
    #[serde(skip)]
    last_redeemed: Option<String>,
    /// Transient: one-shot flag for cancelled actions, not persisted.
    #[serde(skip)]
    last_cancelled: Option<String>,
}

impl Default for ApprovalStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalStore {
    pub fn new() -> Self {
        Self {
            actions: HashMap::new(),
            notifications: VecDeque::new(),
            last_committed: None,
            last_redeemed: None,
            last_cancelled: None,
        }
    }

    /// Enqueue a new approval request and return the id.
    pub fn enqueue(&mut self, tool_name: &str, args_json: &str) -> String {
        // "ap" is short for "approval"
        let id = format!("ap_{}", uuid::Uuid::new_v4().simple());
        let created_at = OffsetDateTime::now_utc();
        let action = ApprovalAction {
            id: id.clone(),
            tool_name: tool_name.to_owned(),
            args_json: args_json.to_owned(),
            commit_reason: None,
            status: ApprovalStatus::Pending,
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
            .ok_or_else(|| anyhow::anyhow!("approval '{id}' not found"))?;
        if !matches!(action.status, ApprovalStatus::Committed) {
            bail!(
                "approval '{id}' is not committed (status: {:?})",
                action.status
            );
        }
        action.status = ApprovalStatus::Approved;
        self.notifications
            .push_back(ApprovalNotification::Approved { id: id.to_owned() });
        Ok(())
    }

    /// Deny a committed action.
    pub fn deny(&mut self, id: &str, reason: &str) -> Result<()> {
        let action = self
            .actions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("approval '{id}' not found"))?;
        if !matches!(action.status, ApprovalStatus::Committed) {
            bail!(
                "approval '{id}' is not committed (status: {:?})",
                action.status
            );
        }
        action.status = ApprovalStatus::Denied;
        action.deny_reason = Some(reason.to_owned());
        self.notifications.push_back(ApprovalNotification::Denied {
            id: id.to_owned(),
            reason: reason.to_owned(),
        });
        Ok(())
    }

    /// Take an approved action for redemption (marks it as redeemed).
    pub fn take_for_redeem(&mut self, id: &str) -> Result<ApprovalAction> {
        let action = self
            .actions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("approval '{id}' not found"))?;
        match &action.status {
            ApprovalStatus::Pending => {
                bail!("approval '{id}' is still pending — commit it first")
            }
            ApprovalStatus::Committed => {
                bail!("approval '{id}' is awaiting approval")
            }
            ApprovalStatus::Denied => {
                let reason = action.deny_reason.as_deref().unwrap_or("unknown");
                bail!("approval '{id}' was denied: {reason}")
            }
            ApprovalStatus::Redeemed => {
                bail!("approval '{id}' has already been redeemed")
            }
            ApprovalStatus::Cancelled => {
                bail!("approval '{id}' was cancelled")
            }
            ApprovalStatus::Approved => {}
        }
        action.status = ApprovalStatus::Redeemed;
        self.last_redeemed = Some(id.to_owned());
        Ok(action.clone())
    }

    /// Cancel an action that is no longer needed.
    ///
    /// Works on any non-terminal state (Pending, Committed, Approved, Denied).
    /// Returns the previous status so the caller can include it in the response.
    pub fn cancel(&mut self, id: &str) -> Result<ApprovalStatus> {
        let action = self
            .actions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("approval '{id}' not found"))?;
        match action.status {
            ApprovalStatus::Redeemed => {
                bail!("approval '{id}' has already been redeemed")
            }
            ApprovalStatus::Cancelled => {
                bail!("approval '{id}' has already been cancelled")
            }
            prev => {
                action.status = ApprovalStatus::Cancelled;
                self.last_cancelled = Some(id.to_owned());
                Ok(prev)
            }
        }
    }

    /// Commit a pending action with the agent's justification.
    ///
    /// Stores the commit_reason and transitions to `Committed` so the action
    /// becomes visible to superiors for approval.
    pub fn commit(&mut self, id: &str, commit_reason: &str) -> Result<()> {
        let action = self
            .actions
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("approval '{id}' not found"))?;
        if !matches!(action.status, ApprovalStatus::Pending) {
            bail!(
                "approval '{id}' is not pending (status: {:?})",
                action.status
            );
        }
        action.commit_reason = Some(commit_reason.to_owned());
        action.status = ApprovalStatus::Committed;
        self.last_committed = Some(ApprovalCommittedInfo {
            id: id.to_owned(),
            tool_name: action.tool_name.clone(),
            args_json: action.args_json.clone(),
            commit_reason: commit_reason.to_owned(),
        });
        Ok(())
    }

    /// Take the info about the most recently committed action (one-shot).
    pub fn take_last_committed(&mut self) -> Option<ApprovalCommittedInfo> {
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

    /// List approvals, optionally filtered by status.
    pub fn list(&self, status_filter: Option<&ApprovalStatus>) -> Vec<ApprovalInfo> {
        self.actions
            .values()
            .filter(|a| status_filter.is_none_or(|f| &a.status == f))
            .map(ApprovalInfo::from)
            .collect()
    }

    /// Check if the queue contains an action with the given id.
    pub fn contains(&self, id: &str) -> bool {
        self.actions.contains_key(id)
    }

    /// Look up a single action by id.
    pub fn get(&self, id: &str) -> Option<ApprovalInfo> {
        self.actions.get(id).map(ApprovalInfo::from)
    }

    /// Drain all pending notifications (for runner context injection).
    pub fn drain_notifications(&mut self) -> Vec<ApprovalNotification> {
        self.notifications.drain(..).collect()
    }
}

/// Format an approval-deferred tool result JSON returned to the LLM.
pub fn approval_result_json(id: &str, tool_name: &str) -> String {
    serde_json::to_string(&ApprovalDeferredResponse {
        ok: true,
        pending_approval: true,
        tool_name: tool_name.to_owned(),
        id: id.to_owned(),
        next_steps: "This tool call was deferred because it requires approval. \
            Call approval_commit with the id and a justification for why it is necessary, \
            or approval_cancel to abandon it."
            .to_owned(),
    })
    .unwrap_or_else(|_| r#"{"ok":true,"pending_approval":true}"#.to_owned())
}

#[derive(Serialize)]
struct ApprovalDeferredResponse {
    ok: bool,
    pending_approval: bool,
    tool_name: String,
    id: String,
    next_steps: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_commit_approve_redeem() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("shell_session_exec", r#"{"command":"rm -rf /tmp"}"#);
        assert!(id.starts_with("ap_"));

        let info = q.list(None);
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].status, ApprovalStatus::Pending);
        assert!(info[0].created_at <= OffsetDateTime::now_utc());

        q.commit(&id, "need to clean up temp dir").unwrap();
        assert_eq!(q.list(None)[0].status, ApprovalStatus::Committed);
        assert_eq!(
            q.list(None)[0].commit_reason.as_deref(),
            Some("need to clean up temp dir")
        );

        q.approve(&id).unwrap();
        assert_eq!(q.list(None)[0].status, ApprovalStatus::Approved);

        let action = q.take_for_redeem(&id).unwrap();
        assert_eq!(action.tool_name, "shell_session_exec");
        assert_eq!(q.list(None)[0].status, ApprovalStatus::Redeemed);
    }

    #[test]
    fn deny_prevents_redeem() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        q.commit(&id, "justification").unwrap();
        q.deny(&id, "no").unwrap();
        assert!(q.take_for_redeem(&id).is_err());
    }

    #[test]
    fn cancel_pending() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        let prev = q.cancel(&id).unwrap();
        assert_eq!(prev, ApprovalStatus::Pending);
        assert_eq!(q.list(None)[0].status, ApprovalStatus::Cancelled);
    }

    #[test]
    fn cancel_committed() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        q.commit(&id, "justification").unwrap();
        let prev = q.cancel(&id).unwrap();
        assert_eq!(prev, ApprovalStatus::Committed);
        assert_eq!(q.list(None)[0].status, ApprovalStatus::Cancelled);
    }

    #[test]
    fn cancel_approved() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        q.commit(&id, "justification").unwrap();
        q.approve(&id).unwrap();
        let prev = q.cancel(&id).unwrap();
        assert_eq!(prev, ApprovalStatus::Approved);
        assert_eq!(q.list(None)[0].status, ApprovalStatus::Cancelled);
    }

    #[test]
    fn cancel_denied() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        q.commit(&id, "justification").unwrap();
        q.deny(&id, "no").unwrap();
        let prev = q.cancel(&id).unwrap();
        assert_eq!(prev, ApprovalStatus::Denied);
        assert_eq!(q.list(None)[0].status, ApprovalStatus::Cancelled);
    }

    #[test]
    fn cannot_cancel_redeemed() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        q.commit(&id, "justification").unwrap();
        q.approve(&id).unwrap();
        q.take_for_redeem(&id).unwrap();
        assert!(q.cancel(&id).is_err());
    }

    #[test]
    fn cannot_cancel_already_cancelled() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        q.cancel(&id).unwrap();
        assert!(q.cancel(&id).is_err());
    }

    #[test]
    fn cannot_approve_non_committed() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        // Pending → cannot approve
        assert!(q.approve(&id).is_err());
        let _ = q.cancel(&id).unwrap();
        // Cancelled → cannot approve
        assert!(q.approve(&id).is_err());
    }

    #[test]
    fn cannot_redeem_pending_or_committed() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        assert!(q.take_for_redeem(&id).is_err());
        q.commit(&id, "justification").unwrap();
        assert!(q.take_for_redeem(&id).is_err());
    }

    #[test]
    fn cannot_commit_non_pending() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        q.commit(&id, "first").unwrap();
        // Already committed
        assert!(q.commit(&id, "second").is_err());
    }

    #[test]
    fn take_last_committed_is_one_shot() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        q.commit(&id, "justification").unwrap();
        let info = q.take_last_committed().unwrap();
        assert_eq!(info.id, id);
        assert_eq!(info.commit_reason, "justification");
        assert!(q.take_last_committed().is_none());
    }

    #[test]
    fn drain_notifications() {
        let mut q = ApprovalStore::new();
        let id1 = q.enqueue("t1", "{}");
        let id2 = q.enqueue("t2", "{}");
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
        let mut q = ApprovalStore::new();
        let id1 = q.enqueue("t1", "{}");
        let id2 = q.enqueue("t2", "{}");
        q.commit(&id1, "j1").unwrap();
        q.commit(&id2, "j2").unwrap();
        q.approve(&id1).unwrap();
        q.deny(&id2, "no").unwrap();

        let approved = q.list(Some(&ApprovalStatus::Approved));
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].id, id1);

        let denied = q.list(Some(&ApprovalStatus::Denied));
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].deny_reason.as_deref(), Some("no"));

        let pending = q.list(Some(&ApprovalStatus::Pending));
        assert_eq!(pending.len(), 0);

        let committed = q.list(Some(&ApprovalStatus::Committed));
        assert_eq!(committed.len(), 0);
    }

    #[test]
    fn not_found_errors() {
        let mut q = ApprovalStore::new();
        assert!(q.approve("nonexistent").is_err());
        assert!(q.deny("nonexistent", "r").is_err());
        assert!(q.take_for_redeem("nonexistent").is_err());
        assert!(q.cancel("nonexistent").is_err());
        assert!(q.commit("nonexistent", "r").is_err());
    }

    #[test]
    fn contains_checks() {
        let mut q = ApprovalStore::new();
        let id = q.enqueue("t", "{}");
        assert!(q.contains(&id));
        assert!(!q.contains("nonexistent"));
    }

    #[test]
    fn info_has_tool_call_content() {
        let mut q = ApprovalStore::new();
        q.enqueue("shell_session_exec", r#"{"command":"ls"}"#);
        let info = &q.list(None)[0];
        assert_eq!(info.content.tool_name, "shell_session_exec");
        assert_eq!(info.content.arguments["command"], "ls");
    }
}
