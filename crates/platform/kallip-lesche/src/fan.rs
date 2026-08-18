//! Membership fan-out: deliver an envelope (and membership-changed hints) to
//! every member of a room.
//!
//! This generalizes the relay's strict-pair routing (one owner <-> one tagma)
//! to multi-member rooms: a message reaches every member the sender is not.
//! Routing is by [`ParticipantKind`]: Human members receive via their app
//! event-stream, Agent members via their tunnel. The relay holds the registry
//! read lock; `broadcast::send` is non-async, so the no-`await`-under-lock
//! discipline holds.
//!
//! A member with no live tunnel/stream (offline) is counted as `missed`, not an
//! error: the relay stores the ciphertext so an offline member pulls the missed
//! message on reconnect. The caller decides whether zero-delivered
//! is a 503 (no one reachable).

use kallip_agora_common::ids::ParticipantKind;
use kallip_lesche_common::event::LescheEvent;
use kallip_lesche_common::message::Envelope;
use kallip_lesche_common::rooms::{MemberId, RoomId, RoomMember, RoomMembership};
use kallip_lesche_common::tunnel::TunnelInbound;

use crate::state::Registry;

/// The result of fanning an envelope to every room member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanOut {
    /// Members a live tunnel/stream existed for (the send succeeded).
    pub delivered: usize,
    /// Members that were offline (no live tunnel/stream, or a closed channel).
    pub missed: usize,
}

/// Deliver `envelope` to every member of `membership` except the sender, under
/// the caller's registry read lock. Human members receive via their app
/// event-stream; Agent members via their tunnel. The sender is excluded by
/// participant-id equality. Returns the delivery counts.
pub fn fan_envelope(
    registry: &Registry,
    membership: &RoomMembership,
    envelope: &Envelope,
) -> FanOut {
    let mut delivered = 0;
    let mut missed = 0;
    // The envelope sender is the wire `ParticipantId`; the membership atoms carry
    // the room-domain `MemberId`. They share a UUID value, so compare in MemberId
    // space (one conversion) rather than at every membership atom.
    let sender_mid = MemberId::from(envelope.sender.id.clone());

    for member in &membership.members {
        // Skip the sender (it authored the envelope).
        if member.id == sender_mid {
            continue;
        }
        let ok = match member.kind {
            ParticipantKind::Human => registry.app_stream_by_member(&member.id).is_some_and(|tx| {
                tx.send(LescheEvent::Envelope {
                    envelope: envelope.clone(),
                })
                .is_ok()
            }),
            ParticipantKind::Agent => registry.presence_by_member(&member.id).is_some_and(|p| {
                p.tx.send(TunnelInbound::Envelope {
                    envelope: envelope.clone(),
                })
                .is_ok()
            }),
        };
        if ok {
            delivered += 1;
        } else {
            missed += 1; // offline (no live stream/tunnel) or a closed channel.
        }
    }

    FanOut { delivered, missed }
}

/// Deliver a membership-changed hint to every listed member with a live channel,
/// under the caller's registry read lock. Routing is by kind, mirroring
/// [`fan_envelope`]: Agent members receive a transient `Wake` on their tunnel
/// (the room-membership poll pump's immediate first tick on reconnect backstops
/// offline agents); Human members receive a `RoomMembershipChanged` event on
/// their app stream (the frontend's roster-refresh trigger). Both are transient
/// -- NOT buffered for offline members. Returns the count of members reached
/// (online + live channel); offline members are silently skipped.
pub fn deliver_membership_changed(
    registry: &Registry,
    room_id: &RoomId,
    members: &[RoomMember],
) -> usize {
    let wake = TunnelInbound::Wake;
    let event = LescheEvent::RoomMembershipChanged {
        room_id: room_id.clone(),
    };
    let mut delivered = 0;
    for member in members {
        let ok = match member.kind {
            ParticipantKind::Agent => registry
                .presence_by_member(&member.id)
                .is_some_and(|p| p.tx.send(wake.clone()).is_ok()),
            ParticipantKind::Human => registry
                .app_stream_by_member(&member.id)
                .is_some_and(|tx| tx.send(event.clone()).is_ok()),
        };
        if ok {
            delivered += 1;
        }
    }
    delivered
}

#[cfg(test)]
mod tests {
    //! Fan-out delivers to every non-sender member; offline members are missed,
    //! not errored; the sender is excluded.

    use super::*;
    use crate::test_support::make_state;
    use kallip_agora_common::bytes::Ciphertext;
    use kallip_agora_common::ids::{ChannelId, ParticipantId, TagmaId, TraceId, UserId};
    use kallip_lesche_common::message::Participant;
    use time::OffsetDateTime;

    fn envelope_from(sender: Participant) -> Envelope {
        Envelope {
            channel_id: ChannelId::from("room-1".to_string()),
            sender,
            sequence_n: 1,
            trace_id: TraceId::from("t".to_string()),
            timestamp: OffsetDateTime::now_utc(),
            ciphertext: Ciphertext(vec![1u8; 12]),
        }
    }

    fn membership(users: &[&str], tagmas: &[&str]) -> RoomMembership {
        RoomMembership {
            members: users
                .iter()
                .map(|u| RoomMember {
                    id: MemberId::for_user(&UserId::from((*u).to_string())),
                    kind: ParticipantKind::Human,
                })
                .chain(tagmas.iter().map(|t| RoomMember {
                    id: MemberId::for_tagma(&TagmaId::from((*t).to_string())),
                    kind: ParticipantKind::Agent,
                }))
                .collect(),
            membership_epoch: 1,
        }
    }

    fn human(handle: &str, user: &str) -> Participant {
        Participant {
            id: ParticipantId::for_user(&UserId::from(user.to_string())),
            kind: ParticipantKind::Human,
            handle: handle.to_string(),
            tagma_id: None,
        }
    }

    fn agent(handle: &str, tagma: &str) -> Participant {
        Participant {
            id: ParticipantId::for_tagma(&TagmaId::from(tagma.to_string())),
            kind: ParticipantKind::Agent,
            handle: handle.to_string(),
            tagma_id: None,
        }
    }

    #[tokio::test]
    async fn user_message_fans_to_all_tagmas_and_other_users() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(10));
        let mem = membership(&["alice", "bob"], &["t1", "t2"]);

        // App streams for both users; keep receivers alive so `send` succeeds.
        let mut app_rxs = Vec::new();
        {
            let mut reg = state.write().unwrap();
            for u in ["alice", "bob"] {
                let tx = reg.open_app_stream(&UserId::from(u.to_string()));
                app_rxs.push(tx.subscribe());
            }
        }
        // Tagma presence; keep a receiver per tunnel.
        let mut tunnel_rxs = Vec::new();
        {
            let mut reg = state.registry.write().unwrap();
            for t in ["t1", "t2"] {
                let (tx, _rx) = tokio::sync::broadcast::channel(8);
                tunnel_rxs.push(tx.subscribe());
                reg.register_presence(
                    &TagmaId::from(t.to_string()),
                    UserId::from("alice".to_string()),
                    tx,
                    std::sync::Arc::new(()),
                );
            }
        }

        let env = envelope_from(human("Alice", "alice"));
        let reg = state.read().unwrap();
        let out = fan_envelope(&reg, &mem, &env);
        drop(tunnel_rxs); // keep alive until after the fan
        drop(app_rxs);
        // Delivered to bob (user) + t1, t2 (tagmas); alice excluded.
        assert_eq!(out.delivered, 3);
        assert_eq!(out.missed, 0);
    }

    #[tokio::test]
    async fn offline_members_are_missed_not_errored() {
        let (state, _control) = make_state(60, std::time::Duration::from_secs(10));
        // One online tagma (t1), one offline (t2); one online user (alice), one
        // offline (carol). Sender is tagma t1 (excluded).
        let mem = membership(&["alice", "carol"], &["t1", "t2"]);
        let tunnel_rx = {
            let mut reg = state.registry.write().unwrap();
            let (tx, _rx) = tokio::sync::broadcast::channel(8);
            let rx = tx.subscribe();
            reg.register_presence(
                &TagmaId::from("t1".to_string()),
                UserId::from("alice".to_string()),
                tx,
                std::sync::Arc::new(()),
            );
            rx
        };
        let app_rx = {
            let mut reg = state.write().unwrap();
            reg.open_app_stream(&UserId::from("alice".to_string()))
                .subscribe()
        };

        let env = envelope_from(agent("Tagma", "t1"));
        let reg = state.read().unwrap();
        let out = fan_envelope(&reg, &mem, &env);
        drop(tunnel_rx);
        drop(app_rx);
        assert_eq!(out.delivered, 1, "only alice reachable");
        assert_eq!(out.missed, 2, "carol + t2 offline");
    }
}
