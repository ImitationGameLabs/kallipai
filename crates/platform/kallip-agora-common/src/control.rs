//! Control-plane messages: tagma enrollment and the initiator<->responder key
//! exchange (the responder is the enrolled tagma).
//!
//! These are the request/response bodies for the agora's control routes. The
//! agora brokers them (forwarding, persistence of the pinned key) but, for the
//! key exchange, cannot derive the resulting shared secret.

use crate::bytes::{Ed25519PublicKey, Ed25519Signature, X25519PublicKey};
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

/// Initiator -> responder (relayed by the agora; the responder is the tagma):
/// start a 1-RTT key exchange for a conversation, carrying the initiator's
/// ephemeral X25519 public key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyExchangeInit {
    pub ephemeral_public: X25519PublicKey,
}

/// Responder -> initiator (relayed by the agora): the responder's ephemeral
/// X25519 public key plus an Ed25519 signature proving ownership of the pinned
/// device key. Both endpoints then derive the same AEAD key via X25519 + HKDF;
/// the agora, having neither private half, cannot.
///
/// The signature is over
/// [`kex_transcript`](crate::proof::kex_transcript)`(responder_id,
/// conversation_id, initiator_ephemeral_public, responder_ephemeral_public)` -
/// i.e. it binds the two ephemeral keys to the responder and conversation, so
/// the initiator can attribute the derived key unambiguously to the pinned
/// identity. The agent bound to the conversation is an internal concern of the
/// responder and is not part of the transcript. The initiator reconstructs this
/// same transcript to verify.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyExchangeResponse {
    pub ephemeral_public: X25519PublicKey,
    pub signature: Ed25519Signature,
}
