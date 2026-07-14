//! Wire types shared by `kallip-agora` (the cloud relay), `kallip-herald` (the
//! host connector), and eventually the app.
//!
//! Design split: the agora reads only routing metadata ([`message::Envelope`]);
//! the E2E payload ([`message::TunnelFrame`]) and the crypto material ([`bytes`])
//! are opaque to it and are decrypted only by the endpoints. The one exception
//! is [`proof`]: the signed-proof transcripts + their *public-key* verifiers
//! live here so the agora (verifier), the herald (signer), and the app SDK
//! share a single contract. No private-key material ever lives in this crate.

pub mod bytes;
pub mod control;
pub mod event;
pub mod herald;
pub mod ids;
pub mod message;
pub mod proof;
