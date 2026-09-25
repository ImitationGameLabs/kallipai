//! Inbound frames the relay pushes down a tagma's tunnel. The tunnel is the
//! tagma's only inbound channel, so it carries forwarded data-plane envelopes
//! and app-initiated key-exchange inits (the control channel that runs *before*
//! a conversation has an E2E key).

use crate::control::KeyExchangeInit;
use crate::message::Envelope;
use kallipai_archeion_common::ids::ConversationId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TunnelInbound {
    /// A forwarded E2E envelope for a conversation this tagma owns.
    Envelope { envelope: Envelope },
    /// An app wants to establish a conversation E2E key. The tagma derives the
    /// shared secret and replies with a signed
    /// [`crate::control::KeyExchangeResponse`]. The agent that backs the
    /// conversation is the tagma's own concern and is not carried here.
    KeyExchange {
        conversation_id: ConversationId,
        init: KeyExchangeInit,
    },
    /// A best-effort hint that a room's membership changed, so the tagma should
    /// refresh its joined-rooms cache immediately instead of waiting for the
    /// next poll tick. A Wake is transient and NOT buffered -- an offline tagma
    /// misses it and relies on the room-membership pump's immediate first tick
    /// on reconnect. Fanned to every live tagma of the changed room. Carries no
    /// payload: the tagma re-fetches its full joined-rooms set on receipt.
    Wake,
    /// A manage-plane REST request relayed from the app (via the lesche
    /// reverse-proxy) for the tagma to execute against its manage router.
    /// Unlike [`TunnelInbound::Envelope`], this frame is NOT E2E-encrypted:
    /// manage metadata is deliberately visible to the relay (TLS transport +
    /// device-proof tunnel auth is the trust base; message content stays on
    /// the envelope path). The tagma enforces a frame-surface allowlist
    /// (low/medium-sensitivity routes only; anything unlisted is a 404) so
    /// prompt-bearing routes never traverse this frame in plaintext.
    ManageRest {
        /// Distributed-trace id minted by the lesche reverse proxy per
        /// proxied request -- a fresh UUID, so traces stay collision-free
        /// across tunnel reconnects (req_id restarts at zero on
        /// reconnect; a trace must not).
        req_id: u64,
        method: String,
        path: String,
        trace: kallipai_archeion_common::ids::TraceId,
        body: serde_json::Value,
    },
    /// A best-effort per-face hint that the lesche's live-subscriber count
    /// for `face` crossed the zero <-> non-zero boundary: `true` means at
    /// least one client is reading that face (push it), `false` means
    /// nobody is listening (suppress the face's pushes, save the work).
    /// Like [`TunnelInbound::Wake`], the hint is transient and NOT
    /// buffered: an offline tagma misses it, so the lesche re-sends the
    /// current per-face truth when the tunnel re-establishes. Face
    /// defaults differ tagma-side: the projection gate fails toward saving
    /// resources (`false`, with a tunnel-up full first shot as the
    /// carrier), the status gate defaults OPEN (an open default
    /// degrades to the shipped always-push semantics and one piggyback
    /// closes an erroneously-open gate, while a wrongly-closed gate has no
    /// carrier). The wire kind renames `subscription_hint` -> `owner_sub`
    /// with the old kind kept as an alias: a legacy frame carries no face
    /// and parses as the projection hint it was (the reverse
    /// window). Carries no face data: the tagma recomputes the snapshot
    /// itself on the next push.
    #[serde(rename = "owner_sub", alias = "subscription_hint")]
    OwnerSub {
        #[serde(default)]
        face: Face,
        active: bool,
    },
}
/// Which plaintext-metadata face a subscription hint governs. One enum
/// so the hint frame and the `/upstream` piggyback counts name the same
/// faces (the two control planes cannot drift on face names).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Face {
    /// The projection SSE read model (`/tagmata/{id}/state/events`).
    #[default]
    Projection,
    /// The owner's live app stream (`/me/events`), the status carrier.
    Status,
}

/// The plaintext reply to a [`TunnelInbound::ManageRest`] frame: the tagma
/// POSTs this back to the lesche, which resolves the pending proxy request.
/// Plaintext by design -- manage metadata is the relay-visible surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManageRestReply {
    pub req_id: u64,
    pub status: u16,
    pub body: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_round_trips() {
        let frame = TunnelInbound::Wake;
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"kind\":\"wake\""), "{json}");
        let back: TunnelInbound = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TunnelInbound::Wake));
    }

    /// The owner_sub hint round-trips per face with its snake_case kind
    /// tag, face value, and active flag (the fifth inbound frame).
    #[test]
    fn owner_sub_round_trips_per_face() {
        for (face, kind) in [(Face::Status, "status"), (Face::Projection, "projection")] {
            let frame = TunnelInbound::OwnerSub { face, active: true };
            let json = serde_json::to_string(&frame).unwrap();
            assert!(
                json.contains("\"kind\":\"owner_sub\"")
                    && json.contains(&format!("\"face\":\"{kind}\""))
                    && json.contains("\"active\":true"),
                "{json}"
            );
            let back: TunnelInbound = serde_json::from_str(&json).unwrap();
            assert!(matches!(back, TunnelInbound::OwnerSub { face: f, active: true } if f == face));
        }
    }

    /// Mixed-version reverse window: a legacy frame -- the old
    /// kind, no face field -- parses as the projection hint it was, via
    /// the kind alias plus the Face serde default.
    #[test]
    fn pre_p6_subscription_hint_parses_as_projection_owner_sub() {
        let legacy = "{\"kind\":\"subscription_hint\",\"active\":true}";
        let back: TunnelInbound = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            back,
            TunnelInbound::OwnerSub {
                face: Face::Projection,
                active: true
            }
        ));
    }
}
