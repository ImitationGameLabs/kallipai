//! Identifier newtypes for the agora subsystem.
//!
//! Each is a thin wrapper over a UUID string, defined via
//! [`kallip_common::id_type!`] (the same macro behind `AgentId`).

use kallip_common::id_type;
use uuid::Uuid;

id_type! {
    /// Unique identifier for a registered agent tagma (one `kallip-tagma` instance).
    TagmaId
}
id_type! {
    /// Unique identifier for a user account.
    UserId
}
id_type! {
    /// Unique identifier for a conversation.
    ConversationId
}
id_type! {
    /// Unique identifier for a persistent multi-member chat room. Distinct
    /// from [`ConversationId`], the bilateral 1:1 conversation key.
    RoomId
}
id_type! {
    /// Distributed-trace identifier propagated on envelopes. The agora passes it
    /// through unchanged so relay and endpoints can be correlated at the telemetry
    /// backend.
    TraceId
}
id_type! {
    /// The cross-transport conversation-sender identity. A user or a tagma, as
    /// seen by the conversation layer -- the `sender` stamped on every live
    /// [`crate::participant::Participant`] envelope on BOTH transports (the
    /// bilateral 1:1 path and multi-member rooms), persisted by the tagma
    /// daemon's chat history, and the key of the relay's shared presence
    /// registry. Derived deterministically from the underlying platform id
    /// ([`UserId`] / [`TagmaId`]) via [`ParticipantId::for_user`] /
    /// [`ParticipantId::for_tagma`], so no mapping table is needed; external
    /// agents mint a random one at enrollment. The daemon-internal `AgentId`
    /// never crosses into this type, so the agent-free boundary is preserved.
    ///
    /// The room domain addresses its members by the related [`MemberId`] -- a
    /// room-clothing alias over this same derivation -- so room code reads as
    /// `member`, not as the near-synonym clash `member's ParticipantId`. The two
    /// convert freely ([`From`]); see [`MemberId`].
    ParticipantId
}

/// The room-domain member identity: a [`ParticipantId`] in room clothing. Rooms
/// address their members by this id (`RoomMember`, `room_members.member_id`, the
/// roster, room-presence fan-out). It is the SAME derived UUID as the sender's
/// [`ParticipantId`] -- [`MemberId::for_user`] / [`MemberId::for_tagma`] reuse the
/// `ParticipantId` derivation byte-for-byte -- so it converts freely at the few
/// seams where the room layer meets the shared transport identity (building the
/// wire `Envelope.sender`, the presence-registry lookups). It exists so room code
/// is member-native and never reads as `member's ParticipantId`.
///
/// Wire-transparent: `#[serde(transparent)]` over [`ParticipantId`] serializes to
/// the same UUID string, so the SSE/HTTP shapes are unchanged.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct MemberId(ParticipantId);

impl MemberId {
    /// Derive the member id for a user. Same value as [`ParticipantId::for_user`].
    pub fn for_user(user_id: &UserId) -> Self {
        Self(ParticipantId::for_user(user_id))
    }

    /// Derive the member id for a tagma. Same value as [`ParticipantId::for_tagma`].
    pub fn for_tagma(tagma_id: &TagmaId) -> Self {
        Self(ParticipantId::for_tagma(tagma_id))
    }
}

impl From<ParticipantId> for MemberId {
    fn from(pid: ParticipantId) -> Self {
        Self(pid)
    }
}

impl From<MemberId> for ParticipantId {
    fn from(mid: MemberId) -> Self {
        mid.0
    }
}

impl From<String> for MemberId {
    fn from(s: String) -> Self {
        Self(ParticipantId::from(s))
    }
}

impl AsRef<str> for MemberId {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for MemberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Which kind of room participant an identity is. `Human` is a signed-in user
/// (WebAuthn); `Agent` is an automated participant -- a platform-native tagma or
/// a future external agent. The daemon-internal agent/team distinction never
/// appears here: this is the room-layer kind only. Wire labels are
/// `"human"` / `"agent"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantKind {
    Human,
    Agent,
}

impl ParticipantKind {
    /// The wire/storage label for this kind. The single source of truth for the
    /// `"human"` / `"agent"` vocabulary: the serde form, the DB column values,
    /// and the render labels all go through here, so a new variant (or a rename)
    /// lands in one place. Matches the `#[serde(rename_all = "snake_case")]` form.
    pub const fn as_str(self) -> &'static str {
        match self {
            ParticipantKind::Human => "human",
            ParticipantKind::Agent => "agent",
        }
    }

    /// Parse a wire/storage label back into the kind (the inverse of
    /// [`as_str`](Self::as_str)). `None` for an unknown label -- the DB CHECK
    /// constraint and the serde guard make an unknown value a corruption error,
    /// not a runtime branch, so callers `.unwrap_or(ParticipantKind::Agent)`
    /// only as a defensive default.
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "human" => Some(ParticipantKind::Human),
            "agent" => Some(ParticipantKind::Agent),
            _ => None,
        }
    }
}

/// Namespace UUID for the deterministic `ConversationId` <- `TagmaId` derivation.
/// Pinned so the agora (producer of `TagmaView.conversation_id`) and any client
/// reproducing the derivation agree byte-for-byte. Change only by introducing a
/// new namespace and migrating.
const CONVERSATION_NAMESPACE: Uuid = Uuid::from_u128(0xd8e2e7c4_5a91_4b3f_8c2d_6e7a8b9c0d1e);

impl ConversationId {
    /// Derive the stable conversation id for a tagma.
    ///
    /// One tagma owns exactly one conversation (its single channel to its
    /// owner): the conversation id is a v5 UUID over this tagma's id string, so
    /// it is stable across reconnects and agora restarts and requires no
    /// storage. Reconnects re-KEX on the *same* id to rotate the E2E key and
    /// reset the sequence window.
    pub fn for_tagma(tagma_id: &TagmaId) -> Self {
        Self(Uuid::new_v5(&CONVERSATION_NAMESPACE, tagma_id.as_ref().as_bytes()).to_string())
    }
}

/// Distinct v5 namespaces for the two `ParticipantId` derivations. Two separate
/// constants (NOT a shared one) so `ParticipantId::for_user("X")` and
/// `ParticipantId::for_tagma("X")` can never collide. Random external-agent
/// `ParticipantId`s are v4, whose version nibble keeps them disjoint from any v5
/// derivation.
const PARTICIPANT_FOR_USER_NAMESPACE: Uuid = Uuid::from_u128(0xb1f52a094ce74f1a9d603e7c1b04a8f2);
const PARTICIPANT_FOR_TAGMA_NAMESPACE: Uuid = Uuid::from_u128(0xc7a96e135db8472eac412f8d9c15b7e1);

impl ParticipantId {
    /// Derive the room-participant id for a user. Stable across reconnects and
    /// agora restarts; requires no storage.
    pub fn for_user(user_id: &UserId) -> Self {
        Self(Uuid::new_v5(&PARTICIPANT_FOR_USER_NAMESPACE, user_id.as_ref().as_bytes()).to_string())
    }

    /// Derive the room-participant id for a tagma. Stable across reconnects and
    /// agora restarts; requires no storage.
    pub fn for_tagma(tagma_id: &TagmaId) -> Self {
        Self(
            Uuid::new_v5(
                &PARTICIPANT_FOR_TAGMA_NAMESPACE,
                tagma_id.as_ref().as_bytes(),
            )
            .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_tagma_is_deterministic_and_invariant() {
        let t = TagmaId::from("tagma-abc".to_string());
        // Same input -> same id.
        assert_eq!(ConversationId::for_tagma(&t), ConversationId::for_tagma(&t));
        // Different input -> different id.
        let t2 = TagmaId::from("tagma-xyz".to_string());
        assert_ne!(
            ConversationId::for_tagma(&t),
            ConversationId::for_tagma(&t2)
        );
        // Produces a real UUID string.
        ConversationId::for_tagma(&t)
            .as_ref()
            .parse::<Uuid>()
            .expect("derived conversation id is a UUID");
    }

    #[test]
    fn participant_id_for_user_and_tagma_are_deterministic_and_disjoint() {
        let u = UserId::from("user-1".to_string());
        let t = TagmaId::from("user-1".to_string()); // SAME underlying string.
        // Deterministic + stable.
        assert_eq!(ParticipantId::for_user(&u), ParticipantId::for_user(&u));
        assert_eq!(ParticipantId::for_tagma(&t), ParticipantId::for_tagma(&t));
        // The two derivations use distinct namespaces, so even an identical
        // underlying string cannot collide across kinds.
        assert_ne!(
            ParticipantId::for_user(&u),
            ParticipantId::for_tagma(&t),
            "for_user and for_tagma must not collide on the same input string"
        );
        // Different inputs differ.
        let u2 = UserId::from("user-2".to_string());
        assert_ne!(ParticipantId::for_user(&u), ParticipantId::for_user(&u2));
        // Produces a real UUID string.
        ParticipantId::for_user(&u)
            .as_ref()
            .parse::<Uuid>()
            .expect("derived participant id is a UUID");
    }

    #[test]
    fn member_id_matches_participant_id_derivation_and_round_trips() {
        let u = UserId::from("user-1".to_string());
        let t = TagmaId::from("tagma-1".to_string());
        // Same derivation, byte-for-byte.
        assert_eq!(
            MemberId::for_user(&u).as_ref(),
            ParticipantId::for_user(&u).as_ref()
        );
        assert_eq!(
            MemberId::for_tagma(&t).as_ref(),
            ParticipantId::for_tagma(&t).as_ref()
        );
        // Bidirectional From is lossless.
        let pid = ParticipantId::for_user(&u);
        let mid: MemberId = pid.clone().into();
        assert_eq!(mid.as_ref(), pid.as_ref());
        let back: ParticipantId = mid.into();
        assert_eq!(back, pid);
        // From<String> (sea-orm column reads) matches.
        assert_eq!(
            MemberId::from(pid.as_ref().to_string()).as_ref(),
            pid.as_ref()
        );
    }
}
