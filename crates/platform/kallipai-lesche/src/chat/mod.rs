//! The chat domain: the lesche's room and direct-session surfaces.
//!
//! Everything here is room-shaped chat with the durable store as the
//! source of truth: the room/direct tables (`db`), the room routes
//! (`rooms`, `room_management`), two-member direct sessions
//! (`direct`), and the fan-out machinery that delivers room events to
//! live members (`fan`, `room_presence`, `member_identity`,
//! `identity`). The boundary toward the link domain is the
//! [`crate::link`] seam plus the re-exported presence fan below.

pub(crate) mod db;

mod direct;
mod fan;
mod identity;
mod member_identity;
mod room_management;
mod room_presence;
mod rooms;

use axum::Router;

use crate::state::SharedConvState;

/// The chat domain's data-plane routes: rooms, room management, and
/// direct sessions.
pub(crate) fn router() -> Router<SharedConvState> {
    Router::new()
        .merge(rooms::router())
        .merge(room_management::router())
        .merge(direct::router())
}

// Cross-domain seam, link -> chat: the link domain owns the agent SSE
// streams, so it is the side that observes agent liveness; the room layer
// consumes those flips.
pub(crate) use room_presence::fan_member_presence;
