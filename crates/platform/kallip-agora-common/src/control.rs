//! Control-plane messages for tagma enrollment (the responder is the enrolled
//! tagma). These are the request/response bodies for the agora's enroll route;
//! the agora records the pinned device key and requires a signed proof of
//! possession on every tunnel reconnect.
//!
//! The key-exchange handshake (`KeyExchangeInit` / `KeyExchangeResponse`) lives
//! in `kallip-lesche-common` -- it is a data-plane operation the lesche brokers.

use crate::bytes::{Ed25519PublicKey, Ed25519Signature};
use crate::ids::TagmaId;
use serde::{Deserialize, Serialize};

/// `POST /v1/tagmata/enroll` — enroll a tagma with a single-use code and its
/// device key, transitioning a pending tagma to enrolled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollRequest {
    /// Single-use, short-TTL enrollment code, bound to a user.
    pub code: String,
    /// The tagma's pinned Ed25519 device public key. The agora records this and
    /// requires a signed proof of possession on every tunnel reconnect.
    pub device_public_key: Ed25519PublicKey,
    /// Ed25519 signature over
    /// [`enroll_transcript`](crate::proof::enroll_transcript)`(code, device_public_key)`,
    /// proving the tagma holds the private half of `device_public_key`. The
    /// agora verifies this before consuming the code, so a stolen enrollment
    /// code alone cannot pin an attacker-chosen key.
    pub signature: Ed25519Signature,
}

/// Response to a successful enrollment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub tagma_id: TagmaId,
    /// A long-lived bearer token (`sk-tagma-...`) the tagma presents to reopen
    /// its tunnel. Stored at rest only as a SHA-256 hash by the agora.
    pub tagma_token: String,
}
