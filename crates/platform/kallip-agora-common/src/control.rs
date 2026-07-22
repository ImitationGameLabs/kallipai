//! Control-plane messages: tagma enrollment and the app<->herald key exchange.
//!
//! These are the request/response bodies for the agora's control routes. The
//! agora brokers them (forwarding, persistence of the pinned key) but, for the
//! key exchange, cannot derive the resulting shared secret.

use crate::bytes::{Ed25519PublicKey, Ed25519Signature, X25519PublicKey};
use crate::ids::TagmaId;
use serde::{Deserialize, Serialize};

/// `POST /v1/tagmata/enroll` — enroll a herald with a single-use code and its
/// device key, transitioning a pending tagma to enrolled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollRequest {
    /// Single-use, short-TTL enrollment code, bound to a user.
    pub code: String,
    /// The herald's pinned Ed25519 device public key. The agora records this and
    /// requires a signed proof of possession on every tunnel reconnect.
    pub device_public_key: Ed25519PublicKey,
    /// Ed25519 signature over
    /// [`enroll_transcript`](crate::proof::enroll_transcript)`(code, device_public_key)`,
    /// proving the herald holds the private half of `device_public_key`. The
    /// agora verifies this before consuming the code, so a stolen enrollment
    /// code alone cannot pin an attacker-chosen key.
    pub signature: Ed25519Signature,
}

/// Response to a successful enrollment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub tagma_id: TagmaId,
    /// A long-lived bearer token (`sk-tagma-...`) the herald presents to reopen
    /// its tunnel. Stored at rest only as a SHA-256 hash by the agora.
    pub tagma_token: String,
}

/// App -> herald (relayed by the agora): start a 1-RTT key exchange for a
/// conversation, carrying the app's ephemeral X25519 public key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyExchangeInit {
    pub ephemeral_public: X25519PublicKey,
}

/// Herald -> app (relayed by the agora): the herald's ephemeral X25519 public
/// key plus an Ed25519 signature proving ownership of the pinned device key.
/// Both endpoints then derive the same AEAD key via X25519 + HKDF; the agora,
/// having neither private half, cannot.
///
/// The signature is over
/// [`kex_transcript`](crate::proof::kex_transcript)`(tagma_id, conversation_id,
/// app_ephemeral_public, herald_ephemeral_public)` - i.e. it binds the two
/// ephemeral keys to the tagma and conversation, so the app can attribute the
/// derived key unambiguously to the pinned identity. The agent bound to the
/// conversation is an internal concern of the herald and is not part of the
/// transcript. The app reconstructs this same transcript to verify.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyExchangeResponse {
    pub ephemeral_public: X25519PublicKey,
    pub signature: Ed25519Signature,
}
