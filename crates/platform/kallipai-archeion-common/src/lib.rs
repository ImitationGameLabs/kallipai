//! Wire types for the `kallipai-archeion` registry (control plane) plus the
//! foundation it exposes to the rest of the platform: identity newtypes, crypto-byte
//! wrappers, the deputy-guard auth principal, the signed-proof verification
//! primitives, and the control-plane RPC contract (`ControlPlane` trait +
//! `/internal/*` request/response types).
//!
//! The archeion is upstream of the data plane: `kallipai-lesche-common` (the
//! lesche wire types) depends on this crate for the foundation, and the
//! data-plane services reach the archeion over HTTP through clients
//! implementing [`control_plane::ControlPlane`]. Data-plane wire types live
//! in the data-plane commons, not here: envelopes in
//! `kallipai-lesche-common`; tunnel frames, the keryx me/events union, and
//! the KEX handshake in `kallipai-keryx-common`.
//! The room-domain wire types (`RoomId`, `MemberId`, the `/rooms` DTOs, the
//! membership snapshot) live there too; this crate keeps only the
//! platform-wide foundation identities (`ParticipantId`, `ParticipantKind`,
//! `TagmaId`, ...).
//!
//! Design note: the archeion deals only in routing metadata and public-key
//! verification; the E2E payload and the crypto material ([`bytes`]) are
//! opaque to it and are decrypted only by the endpoints. The signed-proof
//! transcripts and their public-key verifiers live across this crate (enroll,
//! plus the shared verify primitive) and `kallipai-lesche-common` (tunnel, kex)
//! so the archeion (verifier), the responder (signer), and the app SDK share one
//! contract. No private-key material ever lives in this crate.

pub mod admin;
pub mod bytes;
pub mod control;
pub mod control_plane;
pub mod ids;
pub mod internal_api;
pub mod participant;
pub mod principal;
pub mod proof;
