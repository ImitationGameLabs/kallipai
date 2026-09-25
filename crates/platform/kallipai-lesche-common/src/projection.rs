//! The manage-plane projection snapshot the tagma pushes to the lesche
//! (a full roster plus the aggregate status counters).
//!
//! Plaintext by design -- manage metadata is the relay-visible surface (same
//! trust base as the [`crate::tunnel::TunnelInbound::ManageRest`] frame). The
//! snapshot is a *pure cache* of tagma state: the lesche stores the latest
//! one under a monotonically increasing seq and serves it to clients even
//! when the tagma is offline (stale read), so the tagma simply re-pushes a
//! fresh full snapshot on tunnel-up -- there is no delta protocol.
//!
//! Deliberately excluded: prompt-bearing
//! text never rides the projection. Roster summaries carry only registry
//! metadata (role/description/activity/duty), and the work-schedule
//! projection is deferred until its storage shape lands, precisely so
//! its prompt fields (`wake_prompt`, `final_warn_prompt`) cannot leak in.

use crate::event::TagmaStatusPayload;
use kallipai_archeion_common::ids::TagmaId;
use kallipai_common::protocol::AgentSummary;
use serde::{Deserialize, Serialize};

/// One full-projection push from a tagma. `push_seq` is the tagma's
/// per-connection push counter (reset on tunnel-up): the lesche uses it to
/// reject out-of-order replays within the same connection generation.
/// The lesche stamps its own store-side `seq` and `updated_at` on receipt;
/// those are per-lesche-store, not per-tagma.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectionSnapshot {
    /// The full agent roster, registry metadata only (no prompt text).
    pub agents: Vec<AgentSummary>,
    /// The aggregate status counters (`snapshot_status` output): root state,
    /// subagent totals, token budget/consumed. Kept as the existing wire
    /// type so the status pump's payload shape is reused verbatim.
    pub status: TagmaStatusPayload,

    /// The tagma's per-connection push counter; see the type doc.
    pub push_seq: u64,

    /// The tagma's work schedule, prompt-free projection (the
    /// `wake_prompt`/`final_warn_prompt` texts never ride the projection).
    /// `None` while the tagma has no schedule.
    pub work_schedule: Option<WorkScheduleProjection>,
}

/// The prompt-free work-schedule projection. `spec` and `status` ride as
/// opaque JSON: the concrete enums are tagma-local and the lesche only
/// stores and relays them, never interprets them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkScheduleProjection {
    pub id: String,
    pub spec: serde_json::Value,
    pub pre_warn_minutes: u32,
    pub final_warn_minutes: u32,
    pub status: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// One change-notification frame on the `/tagmata/{id}/state` SSE stream:
/// stream: the lesche tells subscribed clients *that* a tagma's projection
/// moved (and to which store seq), never *what* moved -- clients re-pull
/// via GET. `tagma_id` is carried even on the per-tagma stream so a future
/// global multiplexed stream keeps the same frame shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectionDirty {
    pub tagma_id: TagmaId,
    pub seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The snapshot rides the plaintext internal surface, but it is still a
    /// persisted-then-served wire shape: pin the field names so a rename
    /// cannot silently break the lesche's stored rows across a deploy.
    #[test]
    fn snapshot_field_names_are_wire_stable() {
        let json = serde_json::json!({
            "agents": [],
            "status": {
                "root_state": "idle",
                "subagents_total": 0,
                "subagents_active": 0,
                "token_budget": 1,
                "token_consumed": 0,
            },
            "push_seq": 7,
            "work_schedule": null,
        });
        let snap: ProjectionSnapshot = serde_json::from_value(json).expect("parses");
        assert!(snap.agents.is_empty());
        assert_eq!(
            snap.status.root_state,
            kallipai_common::protocol::AgentState::Idle
        );
        assert_eq!(snap.status.subagents_total, 0);
        assert_eq!(snap.push_seq, 7);
        assert!(snap.work_schedule.is_none());
    }
}
