//! Wire types shared by `kallip-agora` (the cloud relay), the responder
//! endpoint that holds the E2E device key (deployed as a `kallip-tagma`), and
//! eventually the app (the initiator).
//!
//! Design split: the agora reads only routing metadata ([`message::Envelope`]);
//! the E2E payload ([`message::TagmaRequest`] / [`message::TagmaReply`]) and the
//! crypto material ([`bytes`]) are opaque to it and are decrypted only by the
//! endpoints. The one exception is [`proof`]: the signed-proof transcripts +
//! their *public-key* verifiers live here so the agora (verifier), the responder
//! (signer), and the app SDK share a single contract. No private-key material
//! ever lives in this crate.

pub mod admin;
pub mod bytes;
pub mod control;
pub mod control_plane;
pub mod event;
pub mod ids;
pub mod internal_api;
pub mod message;
pub mod principal;
pub mod proof;
pub mod tunnel;
