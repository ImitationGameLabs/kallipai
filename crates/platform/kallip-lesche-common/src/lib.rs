//! Wire types for the `kallip-lesche` relay (data plane): E2EE conversation
//! envelopes, the tunnel inbound frames, the multiplexed `GET /me/events`
//! SSE union ([`event::LescheEvent`]), the 1-RTT key-exchange handshake, and
//! the room-domain wire types ([`rooms`]: the `/rooms` DTOs, the room
//! identity atoms, the membership snapshot).
//!
//! This crate depends on `kallip-archeion-common` for the foundation the data
//! plane shares with the control plane (identity newtypes, crypto-byte
//! wrappers, the deputy principal, the signed-proof verification primitive).
//! The lesche service reaches the archeion over HTTP through a client implementing
//! `kallip_archeion_common::control_plane::ControlPlane`.
//!
//! The E2E payload ([`message::TagmaRequest`] / [`message::TagmaReply`]) and
//! the crypto material are opaque to the archeion and are decrypted only by the
//! endpoints. No private-key material ever lives in this crate.

pub mod control;
pub mod direct;
pub mod event;
pub mod message;
pub mod projection;
pub mod proof;
pub mod rooms;
pub mod tunnel;
