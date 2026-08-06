//! The room-participant wire types.
//!
//! Two shapes share one identity model:
//! - [`Participant`] -- the envelope sender: an opaque [`ParticipantId`] (see
//!   [`crate::ids`]) paired with a [`ParticipantKind`] and an advisory display
//!   `handle`. Sender metadata carried on every live room envelope; the durable
//!   message row stores only the id/kind and derives the handle at read time.
//! - [`RoomMember`] -- the membership atom: just the id + kind, no handle. The
//!   relay's authorization/fan-out shape, carried by the control-plane RPC type
//!   ([`crate::control_plane::RoomMembership`]) and the tagma discovery view
//!   ([`crate::rooms::TagmaRoomView`]). The user-facing roster view carries the
//!   display-augmented [`crate::rooms::RoomMemberProfile`] instead.
//!
//! They are split because a handle is sender metadata (who sent this line),
//! never membership state (who is allowed in the room): the relay authorizes
//! and fans out purely on id + kind. Both live in this foundation crate (not
//! `kallip-lesche-common`) so the control-plane RPC types and the public DTOs
//! can use them directly.
//!
//! `handle` is advisory: the relay authenticates only the id (via cookie /
//! device key), never the handle. Receivers must sanitize it before interpolating
//! into a prompt header. The relay is the sole authority that stamps a
//! `Participant` onto a live envelope; the durable row never persists one.

use crate::ids::{MemberId, ParticipantId, ParticipantKind, TagmaId};

/// A room participant: the opaque room-layer identity, its kind, and an advisory
/// display handle. See the module docs for the trust model.
///
/// `tagma_id` is carried ONLY for an agent sender (the relay stamps it at send
/// time and the history read resolves it), so the client can deep-link a message
/// header to that tagma's profile without reversing the one-way participant id.
/// `None` for humans and for bilateral envelopes that never set it; serialized
/// only when present, so those payloads stay byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Participant {
    pub id: ParticipantId,
    pub kind: ParticipantKind,
    pub handle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagma_id: Option<TagmaId>,
}

/// A room membership entry: just the identity + kind, with no display handle.
/// The membership atom the relay uses to authorize + fan out: carried by the
/// control-plane RPC type ([`crate::control_plane::RoomMembership`]) and the
/// tagma discovery view ([`crate::rooms::TagmaRoomView`]). The user-facing
/// roster view ([`crate::rooms::RoomRosterView`]) instead carries the
/// display-augmented [`crate::rooms::RoomMemberProfile`]; handles are resolved
/// there (and stamped on envelope senders), never carried on this bare atom.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct RoomMember {
    pub id: MemberId,
    pub kind: ParticipantKind,
}
