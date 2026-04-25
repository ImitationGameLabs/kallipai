//! Deferred action queue for async approval of tool calls.

use std::collections::{HashMap, VecDeque};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Status of a deferred tool action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredStatus {
    Pending,
    Approved,
    Denied { reason: String },
    Redeemed,
    Cancelled,
}

/// A tool action that was deferred pending approval.
#[derive(Clone, Debug)]
pub struct DeferredAction {
    pub request_id: String,
    pub tool_name: String,
    pub args_json: String,
    pub summary: String,
    pub reason: String,
    pub dangerous: bool,
    pub status: DeferredStatus,
}

/// A lightweight snapshot returned by list operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeferredActionInfo {
    pub request_id: String,
    pub tool_name: String,
    pub summary: String,
    pub reason: String,
    pub dangerous: bool,
    pub status: DeferredStatus,
}

/// Notification pushed when an external approval/denial arrives.
#[derive(Clone, Debug)]
pub enum DeferredNotification {
    Approved { request_id: String, summary: String },
    Denied { request_id: String, summary: String, reason: String },
}

/// Info about the most recently enqueued deferred action (consumed once).
#[derive(Clone, Debug)]
pub struct DeferredCreateInfo {
    pub request_id: String,
    pub tool_name: String,
    pub summary: String,
    pub reason: String,
    pub dangerous: bool,
}

/// Queue of deferred tool actions awaiting approval.
///
/// Shared between the executor (enqueue, redeem, cancel) and the daemon
/// routes (approve, deny). The runner drains notifications to inject into
/// the LLM context at the start of each round.
pub struct DeferredQueue {
    actions: HashMap<String, DeferredAction>,
    notifications: VecDeque<DeferredNotification>,
    /// Set by `enqueue`, consumed by `take_last_deferred`.
    last_deferred: Option<DeferredCreateInfo>,
}

impl Default for DeferredQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl DeferredQueue {
    pub fn new() -> Self {
        Self { actions: HashMap::new(), notifications: VecDeque::new(), last_deferred: None }
    }

    /// Enqueue a deferred action and return the request_id.
    pub fn enqueue(
        &mut self,
        tool_name: &str,
        args_json: &str,
        summary: &str,
        reason: &str,
        dangerous: bool,
    ) -> String {
        let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
        let action = DeferredAction {
            request_id: request_id.clone(),
            tool_name: tool_name.to_owned(),
            args_json: args_json.to_owned(),
            summary: summary.to_owned(),
            reason: reason.to_owned(),
            dangerous,
            status: DeferredStatus::Pending,
        };
        self.last_deferred = Some(DeferredCreateInfo {
            request_id: request_id.clone(),
            tool_name: tool_name.to_owned(),
            summary: summary.to_owned(),
            reason: reason.to_owned(),
            dangerous,
        });
        self.actions.insert(request_id.clone(), action);
        request_id
    }

    /// Take the info about the most recently enqueued action (one-shot).
    pub fn take_last_deferred(&mut self) -> Option<DeferredCreateInfo> {
        self.last_deferred.take()
    }

    /// Approve a pending action.
    pub fn approve(&mut self, request_id: &str) -> Result<()> {
        let action = self
            .actions
            .get_mut(request_id)
            .ok_or_else(|| anyhow::anyhow!("deferred action '{request_id}' not found"))?;
        if !matches!(action.status, DeferredStatus::Pending) {
            bail!(
                "deferred action '{request_id}' is not pending (status: {:?})",
                action.status
            );
        }
        action.status = DeferredStatus::Approved;
        self.notifications
            .push_back(DeferredNotification::Approved {
                request_id: request_id.to_owned(),
                summary: action.summary.clone(),
            });
        Ok(())
    }

    /// Deny a pending action.
    pub fn deny(&mut self, request_id: &str, reason: &str) -> Result<()> {
        let action = self
            .actions
            .get_mut(request_id)
            .ok_or_else(|| anyhow::anyhow!("deferred action '{request_id}' not found"))?;
        if !matches!(action.status, DeferredStatus::Pending) {
            bail!(
                "deferred action '{request_id}' is not pending (status: {:?})",
                action.status
            );
        }
        action.status = DeferredStatus::Denied { reason: reason.to_owned() };
        self.notifications.push_back(DeferredNotification::Denied {
            request_id: request_id.to_owned(),
            summary: action.summary.clone(),
            reason: reason.to_owned(),
        });
        Ok(())
    }

    /// Take an approved action for redemption (marks it as redeemed).
    pub fn take_for_redeem(&mut self, request_id: &str) -> Result<DeferredAction> {
        let action = self
            .actions
            .get_mut(request_id)
            .ok_or_else(|| anyhow::anyhow!("deferred action '{request_id}' not found"))?;
        match &action.status {
            DeferredStatus::Pending => {
                bail!("deferred action '{request_id}' is still pending approval")
            }
            DeferredStatus::Denied { reason } => {
                bail!("deferred action '{request_id}' was denied: {reason}")
            }
            DeferredStatus::Redeemed => {
                bail!("deferred action '{request_id}' has already been redeemed")
            }
            DeferredStatus::Cancelled => {
                bail!("deferred action '{request_id}' was cancelled")
            }
            DeferredStatus::Approved => {}
        }
        action.status = DeferredStatus::Redeemed;
        Ok(action.clone())
    }

    /// Cancel a pending action.
    pub fn cancel(&mut self, request_id: &str) -> Result<()> {
        let action = self
            .actions
            .get_mut(request_id)
            .ok_or_else(|| anyhow::anyhow!("deferred action '{request_id}' not found"))?;
        if !matches!(action.status, DeferredStatus::Pending) {
            bail!(
                "deferred action '{request_id}' is not pending (status: {:?})",
                action.status
            );
        }
        action.status = DeferredStatus::Cancelled;
        Ok(())
    }

    /// List deferred actions, optionally filtered by status.
    pub fn list(&self, status_filter: Option<&DeferredStatus>) -> Vec<DeferredActionInfo> {
        self.actions
            .values()
            .filter(|a| {
                status_filter.as_ref().is_none_or(|f| {
                    std::mem::discriminant(&a.status) == std::mem::discriminant(f)
                })
            })
            .map(|a| DeferredActionInfo {
                request_id: a.request_id.clone(),
                tool_name: a.tool_name.clone(),
                summary: a.summary.clone(),
                reason: a.reason.clone(),
                dangerous: a.dangerous,
                status: a.status.clone(),
            })
            .collect()
    }

    /// Drain all pending notifications (for runner context injection).
    pub fn drain_notifications(&mut self) -> Vec<DeferredNotification> {
        self.notifications.drain(..).collect()
    }
}

/// Format a deferred tool result JSON returned to the LLM.
pub fn deferred_result_json(
    request_id: &str,
    tool_name: &str,
    reason: &str,
    dangerous: bool,
) -> String {
    serde_json::json!({
        "ok": true,
        "deferred": true,
        "tool_name": tool_name,
        "request_id": request_id,
        "reason": reason,
        "dangerous": dangerous,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_approve_redeem() {
        let mut q = DeferredQueue::new();
        let id = q.enqueue(
            "shell_session_exec",
            r#"{"command":"rm -rf /tmp"}"#,
            "rm -rf /tmp",
            "destructive",
            true,
        );
        assert!(id.starts_with("req_"));

        let info = q.list(None);
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].status, DeferredStatus::Pending);

        q.approve(&id).unwrap();
        assert_eq!(q.list(None)[0].status, DeferredStatus::Approved);

        let action = q.take_for_redeem(&id).unwrap();
        assert_eq!(action.tool_name, "shell_session_exec");
        assert_eq!(q.list(None)[0].status, DeferredStatus::Redeemed);
    }

    #[test]
    fn deny_prevents_redeem() {
        let mut q = DeferredQueue::new();
        let id = q.enqueue("t", "{}", "test", "reason", false);
        q.deny(&id, "no").unwrap();
        assert!(q.take_for_redeem(&id).is_err());
    }

    #[test]
    fn cancel_pending() {
        let mut q = DeferredQueue::new();
        let id = q.enqueue("t", "{}", "test", "reason", false);
        q.cancel(&id).unwrap();
        assert_eq!(q.list(None)[0].status, DeferredStatus::Cancelled);
    }

    #[test]
    fn cannot_approve_non_pending() {
        let mut q = DeferredQueue::new();
        let id = q.enqueue("t", "{}", "test", "reason", false);
        q.cancel(&id).unwrap();
        assert!(q.approve(&id).is_err());
    }

    #[test]
    fn cannot_redeem_pending() {
        let mut q = DeferredQueue::new();
        let id = q.enqueue("t", "{}", "test", "reason", false);
        assert!(q.take_for_redeem(&id).is_err());
    }

    #[test]
    fn take_last_deferred_is_one_shot() {
        let mut q = DeferredQueue::new();
        q.enqueue("t", "{}", "test", "reason", false);
        assert!(q.take_last_deferred().is_some());
        assert!(q.take_last_deferred().is_none());
    }

    #[test]
    fn drain_notifications() {
        let mut q = DeferredQueue::new();
        let id1 = q.enqueue("t1", "{}", "test1", "r1", false);
        let id2 = q.enqueue("t2", "{}", "test2", "r2", true);
        q.approve(&id1).unwrap();
        q.deny(&id2, "no").unwrap();

        let notifs = q.drain_notifications();
        assert_eq!(notifs.len(), 2);
        assert!(q.drain_notifications().is_empty());
    }

    #[test]
    fn list_filters_by_status() {
        let mut q = DeferredQueue::new();
        let id1 = q.enqueue("t1", "{}", "a", "r", false);
        let _id2 = q.enqueue("t2", "{}", "b", "r", false);
        q.approve(&id1).unwrap();

        let approved = q.list(Some(&DeferredStatus::Approved));
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].request_id, id1);

        let pending = q.list(Some(&DeferredStatus::Pending));
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn not_found_errors() {
        let mut q = DeferredQueue::new();
        assert!(q.approve("nonexistent").is_err());
        assert!(q.deny("nonexistent", "r").is_err());
        assert!(q.take_for_redeem("nonexistent").is_err());
        assert!(q.cancel("nonexistent").is_err());
    }
}
