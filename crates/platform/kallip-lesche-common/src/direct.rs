//! Wire types for the direct-session domain (T↔T 1v1 plaintext chat): the
//! session id derivation, the message payload, and the session/message views
//! served by the lesche and consumed by `kallip-lesche-client`.
//!
//! Direct sessions are server-side plaintext spaces for exactly two tagma
//! agents (v1: same owner, auto-accept), stored in their own lesche table
//! family -- distinct from rooms (multi-member, user-managed) and from the
//! bilateral 1:1 conversation (E2E, user↔tagma). The session id is DERIVED,
//! not assigned: both sides compute the same v4-form UUID from the ordered
//! tagma-id pair with zero storage, which makes create-or-get idempotent and
//! pair-addressed reads work without a lookup round trip.

use kallip_archeion_common::bytes::Ciphertext;
use kallip_archeion_common::ids::TagmaId;
use kallip_common::id_type;
use uuid::Uuid;

id_type! {
    /// The stable identifier of a direct session between two tagma agents.
    /// A v4-form UUID derived (v5 hash, version nibble normalized to 4) from
    /// the canonical ordered pair of the two tagma ids in
    /// [`DIRECT_SESSION_NAMESPACE`], so both endpoints and the lesche agree
    /// byte-for-byte with no storage. The v4 nibble keeps the id in the
    /// plaintext member-set routing domain (see
    /// `kallip_archeion_common::ids::ChannelId`), disjoint from the v5 E2E
    /// bilateral conversation domain.
    DirectSessionId
}

/// Namespace UUID for the deterministic [`DirectSessionId`] <- ordered-pair
/// derivation. Pinned so every derivation site (the lesche create route and
/// any client reproducing it) agrees byte-for-byte. Distinct from the
/// conversation and participant namespaces: a v5 hash under this namespace
/// can never collide with a bilateral `ConversationId` or a `ParticipantId`.
const DIRECT_SESSION_NAMESPACE: Uuid = Uuid::from_u128(0x9c4f_1a2e_5d3b_4e7f_8a6c_1b2d_3e4f_5a6b);

impl DirectSessionId {
    /// Derive the direct-session id for the pair `(a, b)`.
    ///
    /// The pair is CANONICALIZED before hashing: the two tagma ids are
    /// ordered by their raw string bytes (`min`, then `max`), so
    /// `for_pair(x, y) == for_pair(y, x)` -- both endpoints derive the same
    /// id regardless of who initiates, which is what makes create-or-get
    /// idempotent under a double-initiation race. The v5 hash's version
    /// nibble is then normalized from 5 to 4 so the id lands in the
    /// plaintext member-set routing domain (`ChannelId` v4), disjoint from
    /// the E2E bilateral conversation domain (v5).
    pub fn for_pair(a: &TagmaId, b: &TagmaId) -> Self {
        let (min, max) = canonical_pair(a, b);
        // `\n` cannot appear in a tagma id (a UUID string), so the join is
        // collision-free.
        let hashed = Uuid::new_v5(
            &DIRECT_SESSION_NAMESPACE,
            format!("{}\n{}", min.as_ref(), max.as_ref()).as_bytes(),
        );
        // The uuid crate exposes no version-nibble setter, so normalize the
        // nibble directly. In the 128-bit big-endian UUID layout the
        // version field is the high nibble of `time_hi_and_version`, i.e.
        // bits 79..76: 5 (SHA-1) -> 4 (random-format).
        let bits = (hashed.as_u128() & !(0xF_u128 << 76)) | (4_u128 << 76);
        Self(Uuid::from_u128(bits).to_string())
    }
}

/// Canonical (byte-ordered) form of a session's member pair: `(min, max)` by
/// the raw tagma-id strings. This is the single ordering definition that the
/// id derivation and the `direct_sessions.member_a`/`member_b` columns both
/// key on -- the columns store exactly this order, so change them together
/// and only here.
pub fn canonical_pair<'a>(a: &'a TagmaId, b: &'a TagmaId) -> (&'a TagmaId, &'a TagmaId) {
    if a.as_ref() <= b.as_ref() {
        (a, b)
    } else {
        (b, a)
    }
}

/// The neutral file-attachment descriptor for a [`DirectMessage`], canonical
/// in `kallip_common::protocol::agent` -- the one attachment type shared by
/// room, relay, and direct surfaces (plain serde, wire shape
/// `{record_id, name, size}`; the shape test below pins it).
pub use kallip_common::protocol::agent::FileAttachment;

/// The direct-session message payload: the plaintext content one tagma sends
/// to its direct-session peer. Mirrors the room payload's minimalism (a
/// message is text plus an optional file link); stored opaquely by the lesche
/// and decoded only by the endpoints. The optional attachment rides with
/// `serde(default)` + `skip_serializing_if`, so messages without one keep the
/// bare `{"text": ...}` shape byte-for-byte.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirectMessage {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<FileAttachment>,
}

/// The peer end of a direct session, as the caller sees it: the peer's tagma
/// id (for pair-addressed addressing) plus its server-stamped stable handle
/// (the same `<id-prefix>@<owner-username>` form the relay stamps on
/// senders).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirectSessionPeer {
    pub tagma_id: TagmaId,
    pub handle: String,
}

/// One row of a tagma's direct-session list and the create-or-get response:
/// the session plus the OTHER member's identity (the caller is implicit --
/// every route here is caller-scoped).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirectSessionView {
    pub session_id: DirectSessionId,
    pub peer: DirectSessionPeer,
    #[serde(with = "time::serde::iso8601")]
    pub created_at: time::OffsetDateTime,
}

/// One stored direct message, as returned by the session history route and
/// decoded by `kallip-lesche-client`. Mirrors the room history row shape
/// (sender with the server-stamped stable handle; payload bytes the endpoints
/// decode); there is no membership epoch -- direct-session membership is
/// immutable, so the rooms row's cache marker has nothing to mark.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirectMessageView {
    pub seq: i64,
    pub sender: crate::message::Participant,
    pub ciphertext: Ciphertext,
    #[serde(with = "time::serde::iso8601")]
    pub created_at: time::OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagma(s: &str) -> TagmaId {
        TagmaId::from(s.to_string())
    }

    #[test]
    fn for_pair_is_deterministic_and_order_canonical() {
        let a = tagma("6d42c8c5-2e9c-4896-af0e-e714ec3e3560");
        let b = tagma("0e70ad8c-5340-41ba-b106-ea0050d86daf");
        let c = tagma("9438257b-fcee-4be4-8139-b44e95de265a");
        // Same input -> same id, in both orders (the canonicalization).
        assert_eq!(
            DirectSessionId::for_pair(&a, &b),
            DirectSessionId::for_pair(&a, &b)
        );
        assert_eq!(
            DirectSessionId::for_pair(&a, &b),
            DirectSessionId::for_pair(&b, &a)
        );
        // Different pairs -> different ids.
        assert_ne!(
            DirectSessionId::for_pair(&a, &b),
            DirectSessionId::for_pair(&a, &c)
        );
        // Produces a real UUID string.
        Uuid::parse_str(DirectSessionId::for_pair(&a, &b).as_ref())
            .expect("derived direct-session id is a UUID");
    }

    #[test]
    fn derived_id_lands_in_v4_domain_and_stays_off_the_bilateral_space() {
        let a = tagma("6d42c8c5-2e9c-4896-af0e-e714ec3e3560");
        let b = tagma("0e70ad8c-5340-41ba-b106-ea0050d86daf");
        let id = DirectSessionId::for_pair(&a, &b);
        // The version nibble is normalized to 4: the plaintext member-set
        // routing domain, disjoint from the v5 E2E bilateral domain.
        let uuid = Uuid::parse_str(id.as_ref()).unwrap();
        assert_eq!(uuid.get_version(), Some(uuid::Version::Random));
        // A distinct derivation namespace: never a bilateral conversation id.
        assert_ne!(
            id.as_ref(),
            kallip_archeion_common::ids::ConversationId::for_tagma(&a).as_ref()
        );
        assert_ne!(
            id.as_ref(),
            kallip_archeion_common::ids::ConversationId::for_tagma(&b).as_ref()
        );
    }

    #[test]
    fn file_attachment_serializes_to_the_shared_wire_shape() {
        // One attachment type across every surface (room, relay, direct): the
        // wire contract is the plain {record_id, name, size} JSON, pinned here
        // so no serde attribute can silently drift it.
        let fa = FileAttachment {
            record_id: Uuid::from_u128(42),
            name: "notes.txt".to_string(),
            size: 11,
            modality: None,
        };
        let bytes = serde_json::to_vec(&fa).unwrap();
        assert_eq!(
            bytes,
            br#"{"record_id":"00000000-0000-0000-0000-00000000002a","name":"notes.txt","size":11}"#
        );
        assert_eq!(fa, serde_json::from_slice(&bytes).unwrap());
    }

    #[test]
    fn direct_message_without_attachment_keeps_the_bare_shape() {
        let m = DirectMessage {
            text: "hi".to_string(),
            attachment: None,
        };
        assert_eq!(serde_json::to_vec(&m).unwrap(), br#"{"text":"hi"}"#);
        let decoded: DirectMessage = serde_json::from_str(r#"{"text":"hi"}"#).unwrap();
        assert_eq!(decoded.text, "hi");
        assert!(decoded.attachment.is_none());
    }
}
