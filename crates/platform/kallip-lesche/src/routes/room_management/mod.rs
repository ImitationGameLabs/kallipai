//! Room lifecycle + membership management: the `/v1/rooms` management surface,
//! ported from the agora registry into lesche (the chat domain owns its
//! membership graph -- database-per-service).
//!
//! A room is a persistent multi-member channel. Membership is stored in one
//! `room_members` table keyed by an opaque `member_id` (the derived
//! `ParticipantId` of the underlying `user_id`/`tagma_id`) with a `kind`
//! discriminant. Membership changes go through one transaction per change that
//! also bumps `rooms.membership_epoch`, the membership-version counter the relay
//! and its clients key on. The
//! live/revocation-audit split applies: `room_members` holds only active rows;
//! a removal hard-deletes the live row and appends a `room_member_revocations`
//! audit entry.
//!
//! Identity facts (a user exists; a tagma is enrolled; a passkey is owned) are
//! attested through the agora registry's `/internal/*` surface rather than
//! local reads -- lesche never touches the identity tables. The agent-free
//! boundary is preserved: a tagma member is identified on the room surface only
//! by its derived `member_id` (a `ParticipantId`); the underlying `tagma_id`
//! rides as a plain `source_id` string, never as a foreign key.
//!
//! ## Module layout
//!
//! Handlers are split by responsibility into child modules: [`lifecycle`]
//! (create/list), [`roster`] (the user roster snapshot + the tagma discovery
//! poll), [`invites`] (the invite flow), [`members`] (add/remove). Cross-handler
//! helpers live in [`shared`]. Each handler/helper is `pub(super)` so this
//! `protected_router` (the parent) can wire it.

mod invites;
mod lifecycle;
mod members;
mod roster;
mod shared;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use axum::Router;
use axum::routing::{get, post};

use crate::state::SharedConvState;

pub fn router() -> Router<SharedConvState> {
    Router::new()
        .route(
            "/rooms",
            get(lifecycle::list_rooms).post(lifecycle::create_room),
        )
        // The invitee's inbox: their pending invites. A static segment under
        // `/rooms` is safe because room ids are UUIDs (never "invites"), and
        // axum matches static segments before the `{room_id}` param.
        .route("/rooms/invites", get(invites::list_my_invites))
        // Public-room discovery (plaintext, open-access rooms). Same static-
        // segment-safety reasoning as `/rooms/invites`: "public" is never a UUID.
        .route("/rooms/public", get(lifecycle::list_public_rooms))
        // A single room's live roster (members + epoch). Member-only; a
        // non-member gets 404.
        .route("/rooms/{room_id}", get(roster::room_roster))
        .route("/rooms/{room_id}/invites", post(invites::create_invite))
        .route(
            "/rooms/{room_id}/invites/{invite_id}/accept",
            post(invites::accept_invite),
        )
        // Join a public room without an invite (open-access). A private or
        // unknown room is rejected with the same 404 as a missing room
        // (collapsed to avoid leaking existence); private rooms use the invite
        // flow.
        .route("/rooms/{room_id}/join", post(members::join_public_room))
        .route("/rooms/{room_id}/tagmata", post(members::add_tagma))
        // Remove a member (self = leave; owner-of-agent = cross-room pull;
        // creator = admin). Keyed by the opaque member id; see
        // `members::remove_member` for the authorization matrix.
        .route(
            "/rooms/{room_id}/members/{member_id}",
            axum::routing::delete(members::remove_member),
        )
        // A tagma's room discovery (the poll source for the room-membership
        // pump): lists the caller's rooms + each room's membership + whether
        // THIS tagma is the creator. Self-only (path tagma id must match the
        // bearer).
        .route("/tagmata/{tagma_id}/rooms", get(roster::list_tagma_rooms))
        // The owner-side view of one of the caller's tagmata's joined rooms
        // (the "Manage rooms" dialog source). User-cookie + registry-attested
        // ownership; distinct from the tagma-self route above.
        .route(
            "/me/tagmata/{tagma_id}/rooms",
            get(roster::list_my_tagma_rooms),
        )
}
