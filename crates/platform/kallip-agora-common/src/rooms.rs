//! Public HTTP wire types for the `/v1/rooms` and room-discovery surfaces of
//! the `kallip-agora` relay.
//!
//! These DTOs cross the public HTTP API (consumed by `kallip-agora-client` and,
//! later, the app), distinct from [`crate::control_plane`], which is the
//! agora<->lesche RPC contract (the `ControlPlane` trait + `/internal/*`
//! types). A type lives here when a tagma or user client deserializes it
//! directly; it lives in `control_plane` only when the data-plane relay
//! consumes it as a trait return value.

use crate::ids::{MemberId, ParticipantKind, RoomId, TagmaId};
use crate::participant::RoomMember;

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
