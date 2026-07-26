//! Data-plane key exchange (the 1-RTT conversation E2E handshake). The
//! responder is the enrolled tagma; the lesche brokers the init/response but
//! cannot derive the resulting shared secret. The enrollment request/response
//! (a control-plane act) lives in `kallip-agora-common`.

use kallip_agora_common::bytes::{Ed25519Signature, X25519PublicKey};
use serde::{Deserialize, Serialize};

/// Initiator -> responder (relayed by the lesche; the responder is the tagma):
/// start a 1-RTT key exchange for a conversation, carrying the initiator's
/// ephemeral X25519 public key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyExchangeInit {
    pub ephemeral_public: X25519PublicKey,
}

/// Responder -> initiator (relayed by the lesche): the responder's ephemeral
/// X25519 public key plus an Ed25519 signature proving ownership of the pinned
/// device key. Both endpoints then derive the same AEAD key via X25519 + HKDF;
/// the lesche, having neither private half, cannot.
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
