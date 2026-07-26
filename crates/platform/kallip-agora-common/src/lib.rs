//! Wire types for the `kallip-agora` relay (control plane) plus the foundation
//! it exposes to the rest of the platform: identity newtypes, crypto-byte
//! wrappers, the deputy-guard auth principal, the signed-proof verification
//! primitives, and the agora-lesche RPC contract (`ControlPlane` trait +
//! `/internal/*` request/response types).
//!
//! The agora is upstream of the data plane: `kallip-lesche-common` (the lesche
//! wire types) depends on this crate for the foundation, and the lesche service
//! reaches the agora over HTTP through a client implementing
//! [`control_plane::ControlPlane`]. Lesche-specific data-plane types (envelopes,
//! tunnel frames, the me/events SSE union, the KEX handshake) live in
//! `kallip-lesche-common`, not here.
//!
//! Design note: the agora deals only in routing metadata and public-key
//! verification; the E2E payload and the crypto material ([`bytes`]) are
//! opaque to it and are decrypted only by the endpoints. The signed-proof
//! transcripts and their public-key verifiers live across this crate (enroll,
//! plus the shared verify primitive) and `kallip-lesche-common` (tunnel, kex)
//! so the agora (verifier), the responder (signer), and the app SDK share one
//! contract. No private-key material ever lives in this crate.

pub mod admin;
pub mod bytes;
pub mod control;
pub mod control_plane;
pub mod ids;
pub mod internal_api;
pub mod principal;
pub mod proof;
