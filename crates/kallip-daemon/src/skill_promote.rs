//! In-memory store for skill promote requests.
//!
//! Promote requests are about shared resources and need to be visible to root
//! agents, not tied to a single agent's session. The store lives at the daemon
//! level (in `AppState`).
//!
//! In-memory for now — lost on restart. The requesting agent can resubmit.

use std::collections::HashMap;

use kallip_common::agentid::AgentId;
use kallip_common::promote::{CreatePromoteRequest, SkillPromoteRecord, SkillPromoteStatus};

/// Typed errors from [`SkillPromoteStore`] operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("promote request '{0}' not found")]
    NotFound(String),
    #[error("promote request '{id}' is {status} (expected pending)")]
    NotPending {
        id: String,
        status: SkillPromoteStatus,
    },
}

/// Manages skill promote requests across all agents.
pub struct SkillPromoteStore {
    requests: HashMap<String, SkillPromoteRecord>,
}

impl SkillPromoteStore {
    pub fn new() -> Self {
        Self {
            requests: HashMap::new(),
        }
    }

    /// Create a new promote request. Returns the generated ID.
    pub fn create(&mut self, req: CreatePromoteRequest) -> String {
        let id = format!("spr_{}", uuid::Uuid::new_v4().simple());
        let record = SkillPromoteRecord {
            id: id.clone(),
            skill_name: req.skill_name,
            has_existing: req.has_existing,
            new_content: req.new_content,
            old_content: req.old_content,
            description: req.description,
            requested_by: req.requested_by,
            status: SkillPromoteStatus::Pending,
            deny_reason: None,
            created_at: time::OffsetDateTime::now_utc(),
            reviewed_at: None,
        };
        self.requests.insert(id.clone(), record);
        id
    }

    /// Get a pending request for approval processing.
    ///
    /// Validates that the request exists and is still `Pending`. Returns a
    /// clone of the record for the caller to perform file I/O (consistency
    /// check + promotion). The caller must call [`Self::commit_approved`] after
    /// the I/O succeeds to finalize the state transition.
    ///
    /// This two-step approach prevents orphaned `Approved` records when
    /// file I/O fails after the status was already transitioned.
    pub fn get_pending(&self, id: &str) -> Result<SkillPromoteRecord, StoreError> {
        let record = self
            .requests
            .get(id)
            .ok_or_else(|| StoreError::NotFound(id.to_owned()))?;

        if record.status != SkillPromoteStatus::Pending {
            return Err(StoreError::NotPending {
                id: id.to_owned(),
                status: record.status,
            });
        }

        Ok(record.clone())
    }

    /// Finalize the Pending → Approved transition after successful file I/O.
    ///
    /// Must be called only after the promotion has been written to disk.
    /// If the record is no longer Pending (e.g., concurrent deny), returns
    /// an error.
    pub fn commit_approved(&mut self, id: &str) -> Result<(), StoreError> {
        let record = self
            .requests
            .get_mut(id)
            .ok_or_else(|| StoreError::NotFound(id.to_owned()))?;

        if record.status != SkillPromoteStatus::Pending {
            return Err(StoreError::NotPending {
                id: id.to_owned(),
                status: record.status,
            });
        }

        record.status = SkillPromoteStatus::Approved;
        record.reviewed_at = Some(time::OffsetDateTime::now_utc());
        Ok(())
    }

    /// Deny a pending request. Transitions Pending → Denied.
    ///
    /// `reason` is `Option<&str>`: pass `None` when the caller supplied no
    /// reason. The store keeps `None` in the record; the display layer is
    /// responsible for substituting any placeholder text.
    ///
    /// Returns `(requested_by, skill_name)` on success so callers can
    /// send notifications without a second lookup.
    pub fn deny(
        &mut self,
        id: &str,
        reason: Option<&str>,
    ) -> Result<(AgentId, String), StoreError> {
        let record = self
            .requests
            .get_mut(id)
            .ok_or_else(|| StoreError::NotFound(id.to_owned()))?;

        if record.status != SkillPromoteStatus::Pending {
            return Err(StoreError::NotPending {
                id: id.to_owned(),
                status: record.status,
            });
        }

        let requested_by = record.requested_by.clone();
        let skill_name = record.skill_name.clone();

        record.status = SkillPromoteStatus::Denied;
        record.deny_reason = reason.map(str::to_owned);
        record.reviewed_at = Some(time::OffsetDateTime::now_utc());
        Ok((requested_by, skill_name))
    }

    /// List requests, optionally filtered by status.
    pub fn list(&self, status_filter: Option<&SkillPromoteStatus>) -> Vec<&SkillPromoteRecord> {
        self.requests
            .values()
            .filter(|r| status_filter.is_none_or(|filter| &r.status == filter))
            .collect()
    }

    /// Get a single request by ID.
    pub fn get(&self, id: &str) -> Option<&SkillPromoteRecord> {
        self.requests.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_id() -> AgentId {
        AgentId::random()
    }

    /// Shorthand builder for test requests.
    fn make_req() -> CreatePromoteRequest {
        CreatePromoteRequest {
            skill_name: "my-skill".into(),
            has_existing: false,
            new_content: "content".into(),
            old_content: None,
            description: None,
            requested_by: agent_id(),
        }
    }

    #[test]
    fn create_stores_record_with_pending_status() {
        let mut store = SkillPromoteStore::new();
        let id = store.create(CreatePromoteRequest {
            new_content: "---\nname: my-skill\n---\nBody".into(),
            description: Some("test skill".into()),
            ..make_req()
        });
        assert!(id.starts_with("spr_"));

        let record = store.get(&id).unwrap();
        assert_eq!(record.skill_name, "my-skill");
        assert_eq!(record.status, SkillPromoteStatus::Pending);
        assert!(record.old_content.is_none());
    }

    #[test]
    fn approve_transitions_to_approved() {
        let mut store = SkillPromoteStore::new();
        let id = store.create(make_req());
        let pending = store.get_pending(&id).unwrap();
        assert_eq!(pending.status, SkillPromoteStatus::Pending);
        store.commit_approved(&id).unwrap();
        let approved = store.get(&id).unwrap();
        assert_eq!(approved.status, SkillPromoteStatus::Approved);
        assert!(approved.reviewed_at.is_some());
    }

    #[test]
    fn deny_transitions_to_denied() {
        let mut store = SkillPromoteStore::new();
        let id = store.create(make_req());
        store.deny(&id, Some("not ready")).unwrap();
        let record = store.get(&id).unwrap();
        assert_eq!(record.status, SkillPromoteStatus::Denied);
        assert_eq!(record.deny_reason.as_deref(), Some("not ready"));
    }

    #[test]
    fn deny_with_none_reason_stores_none() {
        let mut store = SkillPromoteStore::new();
        let id = store.create(make_req());
        store.deny(&id, None).unwrap();
        let record = store.get(&id).unwrap();
        assert_eq!(record.status, SkillPromoteStatus::Denied);
        assert!(record.deny_reason.is_none());
    }

    #[test]
    fn double_approve_rejected() {
        let mut store = SkillPromoteStore::new();
        let id = store.create(make_req());
        store.get_pending(&id).unwrap();
        store.commit_approved(&id).unwrap();
        let err = store.get_pending(&id).unwrap_err();
        assert!(err.to_string().contains("expected pending"));
    }

    #[test]
    fn deny_after_approve_rejected() {
        let mut store = SkillPromoteStore::new();
        let id = store.create(make_req());
        store.get_pending(&id).unwrap();
        store.commit_approved(&id).unwrap();
        let err = store.deny(&id, Some("too late")).unwrap_err();
        assert!(err.to_string().contains("expected pending"));
    }

    #[test]
    fn list_filters_by_status() {
        let mut store = SkillPromoteStore::new();
        let id1 = store.create(CreatePromoteRequest {
            skill_name: "a".into(),
            new_content: "c".into(),
            ..make_req()
        });
        let _id2 = store.create(CreatePromoteRequest {
            skill_name: "b".into(),
            new_content: "c".into(),
            ..make_req()
        });
        store.get_pending(&id1).unwrap();
        store.commit_approved(&id1).unwrap();

        let pending = store.list(Some(&SkillPromoteStatus::Pending));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].skill_name, "b");

        let approved = store.list(Some(&SkillPromoteStatus::Approved));
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].skill_name, "a");

        let all = store.list(None);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn get_unknown_returns_none() {
        let store = SkillPromoteStore::new();
        assert!(store.get("spr_nonexistent").is_none());
    }
}
