//! Public wire types for the room domain: the `/v1/rooms` and room-discovery
//! surfaces served by the `kallip-lesche` relay (the data plane that owns
//! all room state and policy).
//!
//! These DTOs cross the public HTTP API (consumed by `kallip-lesche-client`
//! and the app), together with the room identity atoms ([`RoomId`],
//! [`MemberId`]) and the membership snapshot ([`RoomMembership`]) the
//! lesche's authorization and fan-out paths consume. Platform-wide identity
//! newtypes (`ParticipantId`, `TagmaId`, ...) stay in
//! `kallip_agora_common::ids` -- this crate builds on that foundation.

use kallip_agora_common::ids::{ParticipantId, ParticipantKind, TagmaId, UserId};
use kallip_common::id_type;

id_type! {
    /// Unique identifier for a persistent multi-member chat room. Distinct
    /// from `kallip_agora_common::ids::ConversationId`, the bilateral 1:1
    /// conversation key.
    RoomId
}

/// The room-domain member identity: a [`ParticipantId`] in room clothing. Rooms
/// address their members by this id ([`RoomMember`], `room_members.member_id`,
/// the roster, room-presence fan-out). It is the SAME derived UUID as the
/// sender's [`ParticipantId`] -- [`MemberId::for_user`] / [`MemberId::for_tagma`]
/// reuse the [`ParticipantId`] derivation byte-for-byte -- so it converts freely
/// at the few seams where the room layer meets the shared transport identity
/// (building the wire `Envelope.sender`, the presence-registry lookups). It
/// exists so room code is member-native and never reads as `member's
/// ParticipantId`.
///
/// Wire-transparent: `#[serde(transparent)]` over [`ParticipantId`] serializes to
/// the same UUID string, so the SSE/HTTP shapes are unchanged.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct MemberId(ParticipantId);

impl MemberId {
    /// Derive the member id for a user. Same value as `ParticipantId::for_user`.
    pub fn for_user(user_id: &UserId) -> Self {
        Self(ParticipantId::for_user(user_id))
    }

    /// Derive the member id for a tagma. Same value as `ParticipantId::for_tagma`.
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

/// A room membership entry: just the identity + kind, with no display handle.
/// The membership atom the lesche uses to authorize + fan out: carried by the
/// membership snapshot ([`RoomMembership`]) and the tagma discovery view
/// ([`TagmaRoomView`]). The user-facing roster view ([`RoomRosterView`])
/// instead carries the display-augmented [`RoomMemberProfile`]; handles are
/// resolved there (and stamped on envelope senders), never carried on this
/// bare atom.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct RoomMember {
    pub id: MemberId,
    pub kind: ParticipantKind,
}

/// A room's live membership snapshot, read by the lesche from its own chat
/// store to authorize senders and fan out envelopes. `members` is the current
/// participants (id + kind); `membership_epoch` is the version counter the
/// roster compares to detect staleness.
#[derive(Debug, Clone)]
pub struct RoomMembership {
    pub members: Vec<RoomMember>,
    pub membership_epoch: i64,
}

/// Room visibility -- the public/private distinction. Immutable after create.
///
/// - [`Visibility::Private`] (the default): invite-only membership. The lesche
///   enforces member access (a non-member gets 404); room content is stored
///   server-side in plaintext.
/// - [`Visibility::Public`]: open-access. Any authenticated user may discover
///   the room and join without an invite.
///
/// Serialized snake_case over the wire. The DB stores [`Visibility::as_str`].
/// `Default` is [`Visibility::Private`] (the pre-existing behavior), so a
/// request body that omits the field creates a private room.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Private,
    Public,
}

impl Visibility {
    /// The stable DB/wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
        }
    }

    /// Parse a wire/DB label. `None` on anything unrecognized.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "private" => Some(Self::Private),
            "public" => Some(Self::Public),
            _ => None,
        }
    }

    /// Decode a stored DB value. An unknown/null value degrades to
    /// [`Visibility::Private`] (the pre-existing default) rather than failing a
    /// read -- a torn future migration never blanks the room surface.
    pub fn from_db(s: &str) -> Self {
        Self::parse(s).unwrap_or(Self::Private)
    }
}

/// One row of a tagma's room discovery (`GET /v1/tagmata/{id}/rooms`): the
/// rooms a tagma belongs to, each with its live membership snapshot and whether
/// THIS tagma is the room's creator. The creator is the strict total-order
/// minimum `(joined_at ASC, member_id ASC)` among the room's live Agent
/// members -- a stable, server-authoritative designation. Serialized over the
/// wire to `kallip-lesche-client`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TagmaRoomView {
    pub room_id: RoomId,
    pub members: Vec<RoomMember>,
    pub membership_epoch: i64,
    /// True when this tagma is the room's creator (the strict-min live Agent
    /// member by `(joined_at, member_id)`).
    pub is_creator: bool,
    /// The room's visibility. The lesche enforces member access; the tagma
    /// routes room payloads through the plaintext codec.
    pub visibility: Visibility,
    /// The room's display name (`None` only for a torn read of a since-deleted
    /// room). Surfaced so a tagma owner can label a room they are not themselves
    /// a member of when managing their agent's joined rooms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A roster member with its server-resolved display identity: the stable
/// `handle` (the same authoritative text the relay stamps on room-message
/// senders: `<id-prefix>@<owner-username>` for an agent, `@<username>` for a
/// human) plus the mutable `label` (an agent's owner-set label, a human's
/// `display_name`). The browser renders `label` prepended to `handle` at render;
/// `label` is never part of the handle. Distinct from [`RoomMember`] (the bare
/// membership atom used for authz/fan-out): only the user-facing roster view
/// carries display identity.
///
/// `online` is the member's live connection state at roster-fetch time (an agent
/// holds a tunnel / a human holds an app stream). It is soft, per-incarnation,
/// in-memory -- never a durable fact. The relay populates it from the live
/// registry; `room_member_online` / `room_member_offline` SSE deltas keep it live
/// between fetches. Only the relay populates it; the agora never constructs a
/// [`RoomRosterView`] (or a `RoomMemberProfile`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoomMemberProfile {
    pub id: MemberId,
    pub kind: ParticipantKind,
    /// The mutable display name (agent label / human display_name). `None` when
    /// the registry did not resolve the member (degrade to the handle alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The stable, unforgeable handle. Always present.
    pub handle: String,
    /// Live connection state at fetch time (agent tunnel / human app stream).
    /// Soft state; the SSE presence deltas are the live layer, this is the
    /// fetch-time ground truth that resyncs them.
    pub online: bool,
    /// The agent's `tagma_id`, so a roster row can deep-link to its profile
    /// without reversing the one-way participant id. `None` for humans;
    /// serialized only when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagma_id: Option<TagmaId>,
}

/// A single room's live membership snapshot (`GET /v1/rooms/{id}`), the
/// user-device analog of one [`TagmaRoomView`] row. Returned to a USER who is a
/// current member of the room; a non-member gets 404. The browser consumes this
/// to display the room's roster. `is_creator` is server-authoritative (the
/// room's `created_by_user_id`), so it survives a refresh (unlike a
/// client-local creator flag). `members` carry server-resolved display identity
/// ([`RoomMemberProfile`]); a headless tagma's discovery view
/// ([`TagmaRoomView`]) stays on bare [`RoomMember`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoomRosterView {
    pub room_id: RoomId,
    pub members: Vec<RoomMemberProfile>,
    pub membership_epoch: i64,
    pub is_creator: bool,
    /// The room's visibility.
    pub visibility: Visibility,
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Pin the Visibility wire vocabulary: every variant's serde label and
    /// the as_str/parse round-trips share one source. An added variant turns
    /// the exhaustiveness sentinel below into a compile error, forcing the
    /// wire decision here rather than a silent serde default.
    #[test]
    fn visibility_wire_labels_are_pinned() {
        let _exhaustive = |v: Visibility| match v {
            Visibility::Private | Visibility::Public => {}
        };

        let all = [
            (Visibility::Private, "private"),
            (Visibility::Public, "public"),
        ];
        for (v, label) in all {
            assert_eq!(serde_json::to_value(v).unwrap(), serde_json::json!(label));
            assert_eq!(
                serde_json::from_value::<Visibility>(serde_json::json!(label)).unwrap(),
                v
            );
            assert_eq!(v.as_str(), label);
            assert_eq!(Visibility::parse(label), Some(v));
            assert_eq!(Visibility::from_db(label), v);
        }
        // Unknown labels: parse is None, from_db degrades to Private.
        assert_eq!(Visibility::parse("public-ish"), None);
        assert_eq!(Visibility::from_db("public-ish"), Visibility::Private);
    }

    /// Pin the /v1/rooms wire shapes: exact key set, Option fields skipped
    /// when None and present when Some. Any field rename/addition or a moved
    /// skip attribute turns a pinned key set red.
    #[test]
    fn rooms_wire_shapes_are_pinned() {
        fn keys(v: &serde_json::Value) -> Vec<&str> {
            let mut k: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
            k.sort_unstable();
            k
        }

        let view = TagmaRoomView {
            room_id: RoomId::from("r1".to_string()),
            members: vec![RoomMember {
                id: MemberId::from("m1".to_string()),
                kind: ParticipantKind::Human,
            }],
            membership_epoch: 3,
            is_creator: false,
            visibility: Visibility::Public,
            name: None,
        };
        assert_eq!(
            keys(&serde_json::to_value(&view).unwrap()),
            [
                "is_creator",
                "members",
                "membership_epoch",
                "room_id",
                "visibility"
            ]
        );
        // Nested RoomMember element keys + ParticipantKind wire labels.
        let view_wire = serde_json::to_value(&view).unwrap();
        assert_eq!(keys(&view_wire["members"][0]), ["id", "kind"]);
        assert_eq!(view_wire["members"][0]["kind"], serde_json::json!("human"));

        let mut named = view.clone();
        named.name = Some("ops".into());
        assert_eq!(
            keys(&serde_json::to_value(&named).unwrap()),
            [
                "is_creator",
                "members",
                "membership_epoch",
                "name",
                "room_id",
                "visibility"
            ]
        );

        let bare = RoomMemberProfile {
            id: MemberId::from("m1".to_string()),
            kind: ParticipantKind::Agent,
            label: None,
            handle: "agent@owner".into(),
            online: true,
            tagma_id: None,
        };
        assert_eq!(
            keys(&serde_json::to_value(&bare).unwrap()),
            ["handle", "id", "kind", "online"]
        );
        let bare_wire = serde_json::to_value(&bare).unwrap();
        assert_eq!(bare_wire["kind"], serde_json::json!("agent"));

        let mut full = bare.clone();
        full.label = Some("Ops bot".into());
        full.tagma_id = Some(TagmaId::from("t1".to_string()));
        assert_eq!(
            keys(&serde_json::to_value(&full).unwrap()),
            ["handle", "id", "kind", "label", "online", "tagma_id"]
        );

        let roster = RoomRosterView {
            room_id: RoomId::from("r1".to_string()),
            members: vec![bare],
            membership_epoch: 7,
            is_creator: true,
            visibility: Visibility::Private,
        };
        assert_eq!(
            keys(&serde_json::to_value(&roster).unwrap()),
            [
                "is_creator",
                "members",
                "membership_epoch",
                "room_id",
                "visibility"
            ]
        );
    }
}
