//! Per-room member-presence fan-out. When a participant (an agent tunnel or a
//! human app stream) connects or disconnects, the relay tells every live HUMAN
//! member of every room that participant belongs to, so each member's roster
//! side-panel updates live. This is the room-member analog of the owner-scoped
//! `TagmaOnline`/`TagmaOffline` pair: same idempotent-set semantics, but fanned
//! to peers across rooms rather than to the owner.
//!
//! Like [`crate::fan`], the relay holds the registry read lock only for the
//! synchronous `broadcast::send` calls; all DB work (membership lookups) happens
//! before the lock is taken, so the no-`await`-under-lock discipline holds.

use std::collections::HashMap;

use kallip_agora_common::ids::{MemberId, ParticipantId, RoomId};
use kallip_lesche_common::event::LescheEvent;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use crate::db::entity::room_members;
use crate::state::SharedConvState;

/// Fan a presence transition for `who` to every live HUMAN member of every room
/// `who` belongs to. Best-effort and off the request path (callers spawn it):
/// presence is soft, in-memory state, and a dropped frame self-heals on the
/// viewer's next roster re-fetch (the roster's `online` field is the fetch-time
/// ground truth).
///
/// Existence/leak bound: membership is read from the DB at T1 and the sends run
/// at T2 under the registry lock. A member who leaves in between may transiently
/// receive a delta for a room they just left -- the client drops it (no open
/// conversation for that room), and only T1-current members are ever addressed,
/// so this is never an existence leak to a non-member. Single-relay, in-process,
/// sub-ms window.
pub(crate) async fn fan_member_presence(
    state: &SharedConvState,
    who: &ParticipantId,
    online: bool,
) {
    let Some(db) = state.db.as_ref() else {
        return;
    };
    // The rooms `who` belongs to (its membership footprint).
    let room_ids: Vec<String> = match room_members::Entity::find()
        .filter(room_members::Column::MemberId.eq(who.as_ref().to_string()))
        .all(db)
        .await
    {
        Ok(rows) => rows.into_iter().map(|r| r.room_id).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "presence fan: participant-rooms query failed");
            return;
        }
    };
    if room_ids.is_empty() {
        return;
    }
    // Every member of those rooms, grouped by room. One batched query (no N+1).
    // `room_ids` is consumed by the `is_in` filter; the per-room iteration below
    // walks `by_room` instead, so no second clone is needed.
    let all_members = match room_members::Entity::find()
        .filter(room_members::Column::RoomId.is_in(room_ids))
        .all(db)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "presence fan: room-members query failed");
            return;
        }
    };
    let mut by_room: HashMap<String, Vec<room_members::Model>> = HashMap::new();
    for m in all_members {
        by_room.entry(m.room_id.clone()).or_default().push(m);
    }

    // Sends under the registry read lock. The guard lives to function return, but
    // every operation in the loop is synchronous (`broadcast::send` is non-async),
    // so lock-discipline invariant #1 (no `.await` under a lock) holds. Registry
    // poison is swallowed deliberately: presence is best-effort soft state, and a
    // 500 here would be wrong (the roster poll resyncs); contrast the roster path,
    // which surfaces poison as a 500 because the roster is authoritative.
    let Ok(reg) = state.read() else {
        return;
    };
    for (room_id, members) in &by_room {
        let ev = if online {
            LescheEvent::RoomMemberOnline {
                room_id: RoomId::from(room_id.clone()),
                member_id: MemberId::from(who.clone()),
            }
        } else {
            LescheEvent::RoomMemberOffline {
                room_id: RoomId::from(room_id.clone()),
                member_id: MemberId::from(who.clone()),
            }
        };
        for m in members {
            // Skip the transitioning participant itself: it does not need its own
            // transition delivered. `who` may be a human (it has an app stream) so
            // the explicit self-check matters; an agent `who` has no app stream and
            // is naturally skipped by the lookup below.
            if m.member_id.as_str() == who.as_ref() {
                continue;
            }
            let mid = MemberId::from(m.member_id.clone());
            // Only live HUMAN members receive `LescheEvent`; agents consume their
            // tunnel (`TunnelInbound`), and `app_stream_by_member` is `None`
            // for agents, so the kind filter is implicit.
            if let Some(tx) = reg.app_stream_by_member(&mid) {
                let _ = tx.send(ev.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Presence fan-out: a connecting participant notifies every live human
    //! member of its rooms; the participant itself and offline members are
    //! skipped; agents are not addressed; a participant in several rooms fans to
    //! each. These need a real membership graph, so they use the testcontainer
    //! `db_state` + `seed_room` (mirroring `room_management/tests.rs`), not the
    //! DB-less `make_state`.

    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::routes::test_support::{db_state, seed_room};
    use kallip_agora_common::ids::{TagmaId, UserId};
    use kallip_lesche_common::event::LescheEvent;
    use kallip_lesche_common::tunnel::TunnelInbound;

    /// Receive one event with a short timeout, or `None` if nothing lands.
    /// Generic over the broadcast payload so it observes both `LescheEvent`
    /// (app streams) and `TunnelInbound` (agent tunnels).
    async fn try_recv<T: Clone>(rx: &mut tokio::sync::broadcast::Receiver<T>) -> Option<T> {
        match tokio::time::timeout(Duration::from_millis(150), rx.recv()).await {
            Ok(Ok(ev)) => Some(ev),
            _ => None,
        }
    }

    #[tokio::test]
    async fn fan_delivers_to_live_human_roommate() {
        let (state, _control, _container) = db_state().await;
        let alice = UserId::from("alice".to_string());
        let bob = UserId::from("bob".to_string());
        let t1 = TagmaId::from("t1".to_string());
        seed_room(
            state.db.as_ref().unwrap(),
            "room-1",
            &alice,
            &[&alice, &bob],
            &[&t1],
        )
        .await;

        // Bob is live (holds an app stream); keep a receiver to observe delivery.
        let mut rx = {
            let mut reg = state.write().unwrap();
            reg.open_app_stream(&bob).subscribe()
        };

        fan_member_presence(&state, &ParticipantId::for_tagma(&t1), true).await;

        let ev = try_recv(&mut rx)
            .await
            .expect("bob receives the tagma online");
        assert!(matches!(
            ev,
            LescheEvent::RoomMemberOnline { ref room_id, ref member_id }
                if room_id.as_ref() == "room-1"
                   && member_id == &MemberId::for_tagma(&t1)
        ));
        // Exactly one event (no duplicate, no offline echo).
        assert!(try_recv(&mut rx).await.is_none());
    }

    #[tokio::test]
    async fn fan_skips_the_transitioning_participant_itself() {
        let (state, _control, _container) = db_state().await;
        let alice = UserId::from("alice".to_string());
        let t1 = TagmaId::from("t1".to_string());
        seed_room(
            state.db.as_ref().unwrap(),
            "room-1",
            &alice,
            &[&alice],
            &[&t1],
        )
        .await;

        // Alice is live; she is also the transitioning participant here.
        let mut rx = {
            let mut reg = state.write().unwrap();
            reg.open_app_stream(&alice).subscribe()
        };

        fan_member_presence(&state, &ParticipantId::for_user(&alice), true).await;
        // Alice does NOT receive her own online transition.
        assert!(try_recv(&mut rx).await.is_none());
    }

    #[tokio::test]
    async fn fan_covers_every_room_the_participant_shares() {
        let (state, _control, _container) = db_state().await;
        let alice = UserId::from("alice".to_string());
        let bob = UserId::from("bob".to_string());
        let t1 = TagmaId::from("t1".to_string());
        seed_room(
            state.db.as_ref().unwrap(),
            "room-a",
            &alice,
            &[&alice, &bob],
            &[&t1],
        )
        .await;
        seed_room(
            state.db.as_ref().unwrap(),
            "room-b",
            &alice,
            &[&alice, &bob],
            &[&t1],
        )
        .await;

        let mut rx = {
            let mut reg = state.write().unwrap();
            reg.open_app_stream(&bob).subscribe()
        };

        fan_member_presence(&state, &ParticipantId::for_tagma(&t1), true).await;

        // One event per shared room, each carrying its own room_id.
        let mut seen: Vec<String> = Vec::new();
        while let Some(ev) = try_recv(&mut rx).await {
            match ev {
                LescheEvent::RoomMemberOnline { room_id, .. } => {
                    seen.push(room_id.as_ref().to_string());
                }
                _ => panic!("unexpected event"),
            }
        }
        seen.sort();
        assert_eq!(seen, vec!["room-a".to_string(), "room-b".to_string()]);
    }

    #[tokio::test]
    async fn fan_delivers_offline_to_live_human_roommate() {
        let (state, _control, _container) = db_state().await;
        let alice = UserId::from("alice".to_string());
        let bob = UserId::from("bob".to_string());
        let t1 = TagmaId::from("t1".to_string());
        seed_room(
            state.db.as_ref().unwrap(),
            "room-1",
            &alice,
            &[&alice, &bob],
            &[&t1],
        )
        .await;

        let mut rx = {
            let mut reg = state.write().unwrap();
            reg.open_app_stream(&bob).subscribe()
        };

        // The offline branch (the OnDrop disconnect path) emits RoomMemberOffline.
        fan_member_presence(&state, &ParticipantId::for_tagma(&t1), false).await;

        let ev = try_recv(&mut rx)
            .await
            .expect("bob receives the tagma offline");
        assert!(matches!(
            ev,
            LescheEvent::RoomMemberOffline { ref room_id, ref member_id }
                if room_id.as_ref() == "room-1"
                   && member_id == &MemberId::for_tagma(&t1)
        ));
        assert!(try_recv(&mut rx).await.is_none());
    }

    #[tokio::test]
    async fn fan_does_not_address_agent_roommates() {
        let (state, _control, _container) = db_state().await;
        let alice = UserId::from("alice".to_string());
        let t1 = TagmaId::from("t1".to_string()); // the transitioning tagma
        let t2 = TagmaId::from("t2".to_string()); // an agent roommate (receiver candidate)
        seed_room(
            state.db.as_ref().unwrap(),
            "room-1",
            &alice,
            &[&alice],
            &[&t1, &t2],
        )
        .await;

        // Alice is live (app stream); t2 holds a live tunnel. Keep both receivers.
        let (mut app_rx, mut tunnel_rx) = {
            let mut reg = state.write().unwrap();
            let app_rx = reg.open_app_stream(&alice).subscribe();
            let (ttx, _) = tokio::sync::broadcast::channel::<TunnelInbound>(8);
            let tunnel_rx = ttx.subscribe();
            reg.register_presence(&t2, UserId::from("owner".to_string()), ttx, Arc::new(()));
            (app_rx, tunnel_rx)
        };

        fan_member_presence(&state, &ParticipantId::for_tagma(&t1), true).await;

        // The human alice receives the transition; the agent t2 does NOT (agents
        // are not addressed via `LescheEvent` -- they consume their tunnel).
        let ev = try_recv(&mut app_rx)
            .await
            .expect("alice receives the tagma online");
        assert!(matches!(ev, LescheEvent::RoomMemberOnline { .. }));
        assert!(
            try_recv::<TunnelInbound>(&mut tunnel_rx).await.is_none(),
            "agent tunnel is silent"
        );
    }
}
