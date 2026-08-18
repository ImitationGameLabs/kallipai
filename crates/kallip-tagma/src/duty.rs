//! Per-agent duty status: tracks whether an agent is on-duty (accepting
//! messages normally) or off-duty (messages buffered to inbox).
//!
//! The duty gate sits at the two external message-delivery paths:
//! `enqueue_prompt` (operator/inter-agent/room messages) and
//! `route_to_superior` (approval notifications). Internal self-notifications
//! (notice_sink, reactivation pre-send, spawn prompt) bypass the gate — they
//! are agent-lifecycle internals, not external message delivery.
//!
//! Default state is OnDuty: agents accept messages unless explicitly set
//! off-duty (by the scheduling engine, or the manual
//! override `PUT /agents/{id}/duty`).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::state::AgentId;

// Re-export the wire type so callers use `crate::duty::DutyStatus`.
pub use kallip_common::protocol::DutyStatus;

/// Shared map of per-agent duty status. Sync read access — the duty check
/// must be non-blocking (called from `try_send` paths). Lock contention is
/// negligible: one lock per message, holding for a HashMap lookup.
#[derive(Debug, Default)]
pub struct DutyStore {
    map: Mutex<HashMap<AgentId, DutyStatus>>,
}

impl DutyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the duty status for an agent. Defaults to [`DutyStatus::OnDuty`]
    /// if no entry exists (agents are on-duty until explicitly set off-duty).
    pub fn get(&self, id: &AgentId) -> DutyStatus {
        self.map
            .lock()
            .expect("duty map poisoned")
            .get(id)
            .copied()
            .unwrap_or_default()
    }

    /// Whether the agent is off-duty (messages should be buffered to inbox).
    pub fn is_off_duty(&self, id: &AgentId) -> bool {
        self.get(id) == DutyStatus::OffDuty
    }

    /// Set the duty status for an agent.
    pub fn set(&self, id: AgentId, status: DutyStatus) {
        self.map
            .lock()
            .expect("duty map poisoned")
            .insert(id, status);
    }

    /// Remove the duty entry for an agent (cleanup on removal).
    pub fn remove(&self, id: &AgentId) {
        self.map.lock().expect("duty map poisoned").remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_on_duty() {
        let store = DutyStore::new();
        let id = AgentId::random();
        assert_eq!(store.get(&id), DutyStatus::OnDuty);
        assert!(!store.is_off_duty(&id));
    }

    #[test]
    fn set_off_duty() {
        let store = DutyStore::new();
        let id = AgentId::random();
        store.set(id.clone(), DutyStatus::OffDuty);
        assert_eq!(store.get(&id), DutyStatus::OffDuty);
        assert!(store.is_off_duty(&id));
    }

    #[test]
    fn set_back_on_duty() {
        let store = DutyStore::new();
        let id = AgentId::random();
        store.set(id.clone(), DutyStatus::OffDuty);
        store.set(id.clone(), DutyStatus::OnDuty);
        assert_eq!(store.get(&id), DutyStatus::OnDuty);
        assert!(!store.is_off_duty(&id));
    }

    #[test]
    fn remove_clears_entry() {
        let store = DutyStore::new();
        let id = AgentId::random();
        store.set(id.clone(), DutyStatus::OffDuty);
        store.remove(&id);
        assert_eq!(store.get(&id), DutyStatus::OnDuty);
    }

    #[test]
    fn agents_isolated() {
        let store = DutyStore::new();
        let a = AgentId::random();
        let b = AgentId::random();
        store.set(a.clone(), DutyStatus::OffDuty);
        assert!(store.is_off_duty(&a));
        assert!(!store.is_off_duty(&b));
    }

    #[test]
    fn duty_status_serde() {
        assert_eq!(
            serde_json::to_string(&DutyStatus::OnDuty).unwrap(),
            "\"onduty\""
        );
        assert_eq!(
            serde_json::to_string(&DutyStatus::OffDuty).unwrap(),
            "\"offduty\""
        );
        assert_eq!(
            serde_json::from_str::<DutyStatus>("\"onduty\"").unwrap(),
            DutyStatus::OnDuty
        );
    }
}
