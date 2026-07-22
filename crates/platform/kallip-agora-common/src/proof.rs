//! Signed-proof transcripts + public-key verifiers, shared by the agora
//! (verifier), the herald (signer), and the future app SDK.
//!
//! Three proofs gate the trust model:
//! - **Enroll proof** (`POST /v1/tagmata`): the herald proves it holds the
//!   private half of the device key it is pinning, so a stolen enrollment code
//!   alone cannot pin an attacker-chosen key.
//! - **Tunnel proof** (`GET /v1/herald/tunnel`): on every (re)connect the herald
//!   proves continued possession of the pinned key, so a stolen long-lived
//!   `tagma_token` alone cannot open a tunnel. The proof is timestamp-bounded
//!   (the agora rejects any timestamp outside `+/- proof_skew_secs`) to defeat
//!   indefinite replay of a captured proof.
//! - **Key-exchange proof**: the herald signs its ephemeral X25519 half (bound
//!   to the tagma and conversation) so the app can attribute the derived key to
//!   the pinned device identity. The agent backing the conversation is a
//!   herald-internal concern and is intentionally not part of the transcript.
//!
//! Every variable-length field is length-prefixed (4-byte big-endian) so the
//! wire contract is unambiguous. This crate performs only public-key
//! `verify_strict`; the signing half lives in the herald/app.

use ed25519_dalek::{Signature, VerifyingKey};

const ENROLL_TAG: &[u8] = b"kallip-agora-enroll-v1";
const TUNNEL_TAG: &[u8] = b"kallip-agora-tunnel-proof-v1";
const KEX_TAG: &[u8] = b"kallip-agora-kex-v1";

/// Why a proof verification failed. Maps to an HTTP status at the route layer
/// (malformed -> 400; invalid -> 401 for the tunnel, 400 for enroll).
#[derive(Debug, thiserror::Error)]
pub enum ProofError {
    #[error("malformed device public key")]
    MalformedKey,
    #[error("malformed signature")]
    MalformedSignature,
    #[error("invalid signature")]
    InvalidSignature,
}

/// Append a 4-byte big-endian length prefix followed by the bytes.
fn framed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("field length fits in u32");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Transcript signed at enrollment: `tag || len(code) || code || device_pubkey`.
pub fn enroll_transcript(code: &str, device_pubkey: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENROLL_TAG.len() + 4 + code.len() + 32);
    out.extend_from_slice(ENROLL_TAG);
    framed(&mut out, code.as_bytes());
    out.extend_from_slice(device_pubkey);
    out
}

/// Transcript signed on every tunnel (re)connect:
/// `tag || len(tagma_id) || tagma_id || unix_secs(8 be)`.
pub fn tunnel_transcript(tagma_id: &str, unix_secs: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(TUNNEL_TAG.len() + 4 + tagma_id.len() + 8);
    out.extend_from_slice(TUNNEL_TAG);
    framed(&mut out, tagma_id.as_bytes());
    out.extend_from_slice(&unix_secs.to_be_bytes());
    out
}

/// Transcript signed in a key-exchange response:
/// `tag || tagma_id || conv_id || app_eph || herald_eph` (each string
/// length-prefixed; the 32-byte ephemeral keys are fixed-width).
pub fn kex_transcript(tagma_id: &str, conv_id: &str, app_eph: &[u8], herald_eph: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(KEX_TAG.len() + 8 + tagma_id.len() + conv_id.len() + 64);
    out.extend_from_slice(KEX_TAG);
    framed(&mut out, tagma_id.as_bytes());
    framed(&mut out, conv_id.as_bytes());
    out.extend_from_slice(app_eph);
    out.extend_from_slice(herald_eph);
    out
}

fn verify(device_pubkey: &[u8], msg: &[u8], sig: &[u8]) -> Result<(), ProofError> {
    let key_bytes: [u8; 32] = device_pubkey
        .try_into()
        .map_err(|_| ProofError::MalformedKey)?;
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| ProofError::MalformedKey)?;
    let signature = Signature::from_slice(sig).map_err(|_| ProofError::MalformedSignature)?;
    key.verify_strict(msg, &signature)
        .map_err(|_| ProofError::InvalidSignature)
}

/// Verify an enrollment proof (signature over [`enroll_transcript`]).
pub fn verify_enroll_proof(device_pubkey: &[u8], code: &str, sig: &[u8]) -> Result<(), ProofError> {
    verify(device_pubkey, &enroll_transcript(code, device_pubkey), sig)
}

/// Verify a tunnel reconnect proof (signature over [`tunnel_transcript`]).
/// The caller checks the timestamp skew separately.
pub fn verify_tunnel_proof(
    device_pubkey: &[u8],
    tagma_id: &str,
    unix_secs: i64,
    sig: &[u8],
) -> Result<(), ProofError> {
    verify(device_pubkey, &tunnel_transcript(tagma_id, unix_secs), sig)
}

/// Verify a key-exchange proof (signature over [`kex_transcript`]).
pub fn verify_kex_proof(
    device_pubkey: &[u8],
    tagma_id: &str,
    conv_id: &str,
    app_eph: &[u8],
    herald_eph: &[u8],
    sig: &[u8],
) -> Result<(), ProofError> {
    verify(
        device_pubkey,
        &kex_transcript(tagma_id, conv_id, app_eph, herald_eph),
        sig,
    )
}

#[cfg(test)]
mod tests {
    //! Lock the exact transcript byte layout (the wire contract for the app SDK)
    //! and exercise accept/reject of every proof.
    //!
    //! Signing uses ed25519-dalek's `SigningKey` directly (the herald's
    //! `DeviceKey` wraps the same primitive), so these tests validate the full
    //! sign->verify contract without depending on the herald crate.

    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn keypair() -> (SigningKey, [u8; 32]) {
        let signing = SigningKey::from_bytes(&[0x42; 32]);
        let public = signing.verifying_key().to_bytes();
        (signing, public)
    }

    #[test]
    fn enroll_transcript_layout_is_exact_and_unambiguous() {
        let t = enroll_transcript("abc", &[0u8; 32]);
        // tag || len(3)be || "abc" || 32 zero bytes
        let mut expect = Vec::new();
        expect.extend_from_slice(ENROLL_TAG);
        expect.extend_from_slice(&3u32.to_be_bytes());
        expect.extend_from_slice(b"abc");
        expect.extend_from_slice(&[0u8; 32]);
        assert_eq!(t, expect);
    }

    #[test]
    fn tunnel_transcript_layout_is_exact_and_unambiguous() {
        let t = tunnel_transcript("tagma-1", 7);
        let mut expect = Vec::new();
        expect.extend_from_slice(TUNNEL_TAG);
        expect.extend_from_slice(&7u32.to_be_bytes());
        expect.extend_from_slice(b"tagma-1");
        expect.extend_from_slice(&7i64.to_be_bytes());
        assert_eq!(t, expect);
    }

    #[test]
    fn length_prefixing_prevents_field_ambiguity() {
        // A tagma_id ending in bytes that look like a length prefix must not be
        // re-parseable as a shorter tagma_id + timestamp.
        let a = tunnel_transcript("AB", 0x4142_4344_4546_4748);
        let b = tunnel_transcript("ABCDEFGH", 0);
        assert_ne!(a, b, "length-prefixing must make transcripts unambiguous");
    }

    #[test]
    fn enroll_proof_round_trips() {
        let (signing, public) = keypair();
        let sig = signing
            .sign(&enroll_transcript("the-code", &public))
            .to_bytes();
        assert!(verify_enroll_proof(&public, "the-code", &sig).is_ok());
    }

    #[test]
    fn enroll_proof_rejects_wrong_code() {
        let (signing, public) = keypair();
        let sig = signing
            .sign(&enroll_transcript("the-code", &public))
            .to_bytes();
        assert!(matches!(
            verify_enroll_proof(&public, "other-code", &sig),
            Err(ProofError::InvalidSignature)
        ));
    }

    #[test]
    fn tunnel_proof_round_trips_and_rejects_replay_on_other_tagma() {
        let (signing, public) = keypair();
        let sig = signing.sign(&tunnel_transcript("tagma-A", 100)).to_bytes();
        assert!(verify_tunnel_proof(&public, "tagma-A", 100, &sig).is_ok());
        assert!(matches!(
            verify_tunnel_proof(&public, "tagma-B", 100, &sig),
            Err(ProofError::InvalidSignature)
        ));
    }

    #[test]
    fn kex_proof_matrix() {
        let (signing, public) = keypair();
        let app_eph = [0xaa; 32];
        let herald_eph = [0xbb; 32];
        let sig = signing
            .sign(&kex_transcript("tagma", "conv", &app_eph, &herald_eph))
            .to_bytes();

        // Happy path.
        assert!(verify_kex_proof(&public, "tagma", "conv", &app_eph, &herald_eph, &sig).is_ok());
        // Wrong conversation.
        assert!(verify_kex_proof(&public, "tagma", "other", &app_eph, &herald_eph, &sig).is_err());
        // Wrong tagma.
        assert!(verify_kex_proof(&public, "other", "conv", &app_eph, &herald_eph, &sig).is_err());
        // Tampered ephemeral key.
        let mut bad_eph = app_eph;
        bad_eph[0] ^= 0xff;
        assert!(verify_kex_proof(&public, "tagma", "conv", &bad_eph, &herald_eph, &sig).is_err());
        // Different device key.
        let other_sig = SigningKey::from_bytes(&[0x99; 32])
            .sign(&kex_transcript("tagma", "conv", &app_eph, &herald_eph))
            .to_bytes();
        assert!(
            verify_kex_proof(&public, "tagma", "conv", &app_eph, &herald_eph, &other_sig).is_err()
        );
    }

    #[test]
    fn malformed_inputs_are_rejected_cleanly() {
        let (signing, public) = keypair();
        let sig = signing.sign(&enroll_transcript("c", &public)).to_bytes();
        // Bad key length.
        assert!(matches!(
            verify_enroll_proof(&[0u8; 10], "c", &sig),
            Err(ProofError::MalformedKey)
        ));
        // Bad signature length.
        assert!(matches!(
            verify_enroll_proof(&public, "c", &[0u8; 10]),
            Err(ProofError::MalformedSignature)
        ));
    }
}
