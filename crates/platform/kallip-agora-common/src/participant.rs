//! The room-participant wire types.
//!
//! One shape carries the identity model shared with the room domain:
//! - [`Participant`] -- the envelope sender: an opaque [`ParticipantId`] (see
//!   [`crate::ids`]) paired with a [`ParticipantKind`] and an advisory display
//!   `handle`. Sender metadata carried on every live room envelope; the durable
//!   message row stores only the id/kind and derives the handle at read time.
//!
//! It is split from the room membership atom (`RoomMember`) because a handle
//! is sender metadata (who sent this line), never membership state (who is
//! allowed in the room): the relay authorizes and fans out purely on id +
//! kind. `RoomMember` and the room identities live with the room domain in
//! `kallip-lesche-common` (`kallip_lesche_common::rooms`).
//!
//! `handle` is advisory: the relay authenticates only the id (via cookie /
//! device key), never the handle. Receivers must sanitize it before interpolating
//! into a prompt header. The relay is the sole authority that stamps a
//! `Participant` onto a live envelope; the durable row never persists one.

use crate::ids::{ParticipantId, ParticipantKind, TagmaId};

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
