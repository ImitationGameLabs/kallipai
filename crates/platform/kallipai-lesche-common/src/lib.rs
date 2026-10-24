//! Wire types for the `kallipai-lesche` relay (data plane): E2EE conversation
//! envelopes, the multiplexed `GET /me/events` SSE union
//! ([`event::LescheEvent`]), the manage-plane projection snapshot
//! ([`projection`]), and the room-domain wire types ([`rooms`]: the
//! `/rooms` DTOs, the room identity atoms, the membership snapshot). The
//! tunnel inbound frames and the key-exchange wire live in
//! `kallipai-keryx-common`, which depends on this crate one-way.
//!
//! This crate depends on `kallipai-archeion-common` for the foundation the data
//! plane shares with the control plane (identity newtypes, crypto-byte
//! wrappers, the deputy principal, the signed-proof verification primitive).
//! The lesche service reaches the archeion over HTTP through a client implementing
//! `kallipai_archeion_common::control_plane::ControlPlane`.
//!
//! The E2E payload ([`message::TagmaRequest`] / [`message::TagmaReply`]) and
//! the crypto material are opaque to the archeion and are decrypted only by the
//! endpoints. No private-key material ever lives in this crate.

pub mod direct;
pub mod event;
pub mod ids;
pub mod message;
pub mod projection;
pub mod proof;
pub mod rooms;
