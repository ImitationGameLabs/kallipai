//! Wire types for the `kallip-lesche` relay (data plane): E2EE conversation
//! envelopes, the tunnel inbound frames, the multiplexed `GET /v1/me/events`
//! SSE union ([`event::LescheEvent`]), the 1-RTT key-exchange handshake, and
//! the room-domain wire types ([`rooms`]: the `/v1/rooms` DTOs, the room
//! identity atoms, the membership snapshot).
//!
//! This crate depends on `kallip-agora-common` for the foundation the data
//! plane shares with the control plane (identity newtypes, crypto-byte
//! wrappers, the deputy principal, the signed-proof verification primitive).
//! The lesche service reaches the agora over HTTP through a client implementing
//! `kallip_agora_common::control_plane::ControlPlane`.
//!
//! The E2E payload ([`message::TagmaRequest`] / [`message::TagmaReply`]) and
//! the crypto material are opaque to the agora and are decrypted only by the
//! endpoints. No private-key material ever lives in this crate.

pub mod control;
pub mod event;
pub mod message;
pub mod proof;
pub mod rooms;
pub mod tunnel;
