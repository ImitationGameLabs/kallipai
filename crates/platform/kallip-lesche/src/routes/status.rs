//! Tagma status relay fan: presence-cache write + owner-stream broadcast.
//!
//! The batched `POST /tagmata/{tagma_id}/upstream` channel demultiplexes
//! Status elements into [`relay_status`], which rebroadcasts the snapshot as
//! an [`LescheEvent::TagmaStatus`] on the owner's app event stream. Status is
//! plaintext and user-scoped, so the lesche can read it -- agent counts and
//! token budget are operator metadata, not conversation content. The relay
//! does not parse or validate the numbers and does not rate-limit; the
//! snapshot cadence is a tagma-side contract.
//!
//! Concurrency: the cache write runs under a registry WRITE lock (it mutates
//! mutates `PresenceEntry::latest_status`; broadcast `send` is synchronous),
//! never co-held with a `ControlPlane` call.

use axum::http::StatusCode;
use kallip_archeion_common::ids::TagmaId;
use kallip_common::protocol::ApiError;
use kallip_lesche_common::event::{LescheEvent, TagmaStatusPayload};
use tracing::debug;

use crate::state::SharedConvState;

/// The batched upstream channel's status fan: presence-cache write +
/// owner-stream broadcast. Single shared implementation so wire paths
/// cannot drift.
pub(super) async fn relay_status(
    state: &SharedConvState,
    tagma_id: TagmaId,
    payload: TagmaStatusPayload,
) -> Result<StatusCode, ApiError> {
    // Resolve the owner from the in-memory presence cache (populated on tunnel
    // open by `register_presence`). The status pump runs only while the tunnel
    // is live, so presence is guaranteed present; a missing entry means the
    // tunnel is gone -- surface 404 rather than silently masking a routing
    // gap. Guard is dropped before any await (lock discipline).
    let (app_stream, meaningful) = {
        // WRITE lock: the cache write mutates `latest_status` (lock discipline
        // discipline still holds -- no awaits inside this CS).
        let mut reg = state.write()?;
        let entry = reg
            .presence_by_tagma_mut(&tagma_id)
            .ok_or_else(|| ApiError::not_found("no live tunnel for tagma"))?;
        // Broadcast throttle: only meaningful transitions fan out to the
        // owner's app stream (see
        // `TagmaStatusPayload::meaningful_transition_from`); the cache
        // write stays unconditional so `GET /tagmata/{id}/status` and
        // the connect flush always serve the freshest relayed values.
        let meaningful = payload.meaningful_transition_from(entry.last_broadcast_status.as_ref());
        if meaningful {
            entry.last_broadcast_status = Some(payload.clone());
        }
        // Unconditional cache write: the snapshot must land even with no
        // live subscribers, or a reconnect's flush starts from the
        // pre-gap value.
        entry.latest_status = Some(payload.clone());
        let owner = entry.owner.clone();
        (reg.app_stream(&owner), meaningful)
    };

    // Ordering: the cache write above completes before this fan-out (the
    // write guard drops at the end of the critical section). The connect
    // flush sends outside the lock, so a flush that read a pre-write
    // snapshot may land after this send and briefly bury the newer
    // frame; the bury is bounded and transient -- the next pump wake
    // supersedes it (last-wins).
    // No live app stream -> silent drop (best-effort). Still 202 so the tagma
    // does not retry; the next periodic snapshot supersedes this one.
    if meaningful
        && let Some(stream) = app_stream
        && stream
            .deliver(LescheEvent::TagmaStatus {
                tagma_id: tagma_id.clone(),
                root_state: payload.root_state,
                subagents_total: payload.subagents_total,
                subagents_active: payload.subagents_active,
                token_budget: payload.token_budget,
                token_consumed: payload.token_consumed,
            })
            .is_ok()
    {
        debug!(tagma = %tagma_id, "status relayed");
    }
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_state, seed_presence};
    use kallip_archeion_common::bytes::Ed25519PublicKey;
    use kallip_archeion_common::ids::{TagmaId, UserId};
    use kallip_common::protocol::AgentState;

    fn user(name: &str) -> UserId {
        UserId::from(name.to_string())
    }

    #[tokio::test]
    async fn relay_status_caches_snapshot_without_subscribers() {
        let (state, control) = make_state(60, std::time::Duration::from_secs(2));
        let owner = user("owner");
        let tagma = TagmaId::from("tagma-1".to_string());
        control.enroll_tagma(
            &tagma,
            owner.clone(),
            Ed25519PublicKey(vec![0u8; 32]),
            "tok",
        );
        let (_t_tx, _id) = seed_presence(&state, &tagma, owner.clone());
        // No app stream opened: the relay is a silent drop for delivery, but
        // the cache write is unconditional so a late-connecting client's
        // me_events flush starts from this snapshot, not the pre-gap value.
        let status = relay_status(
            &state,
            tagma.clone(),
            TagmaStatusPayload {
                root_state: AgentState::Busy,
                subagents_total: 3,
                subagents_active: 2,
                token_budget: 50_000,
                token_consumed: 12_000,
                token_budget_unlimited: false,
            },
        )
        .await
        .expect("relay ok");
        assert_eq!(status, StatusCode::ACCEPTED);
        let cached = state
            .read()
            .unwrap()
            .presence_by_tagma(&tagma)
            .and_then(|e| e.latest_status.clone())
            .expect("snapshot cached despite no subscribers");
        assert_eq!(cached.subagents_total, 3);
        assert_eq!(cached.subagents_active, 2);
    }
}
