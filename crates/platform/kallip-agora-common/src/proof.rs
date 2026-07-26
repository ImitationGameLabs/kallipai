//! Signed-proof transcripts + public-key verifiers, shared by the agora
//! (verifier), the responder (signer), and the future app SDK (the initiator).
//!
//! This crate holds the **enroll** proof (verified at `POST /v1/tagmata`) plus
//! the shared verification primitive ([`verify`]) and error type
//! ([`ProofError`]) that the tunnel and key-exchange proofs (in
//! `kallip-lesche-common`) build on.
//!
//! Every variable-length field is length-prefixed (4-byte big-endian) so the
//! wire contract is unambiguous. This crate performs only public-key
//! `verify_strict`; the signing half lives in the endpoints.

use ed25519_dalek::{Signature, VerifyingKey};

const ENROLL_TAG: &[u8] = b"kallip-agora-enroll-v1";

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

/// Append a 4-byte big-endian length prefix followed by the bytes. Used by the
/// plane-specific transcript builders (enroll here; tunnel/kex in
/// `kallip-lesche-common`) to length-prefix variable-length fields.
pub fn framed(out: &mut Vec<u8>, bytes: &[u8]) {
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

/// Verify a device-key signature strictly, mapping dalek errors into
/// [`ProofError`]. Pure CPU (no async); called by each plane's `verify_*_proof`
/// (enroll here; tunnel/kex in `kallip-lesche-common`).
pub fn verify(device_pubkey: &[u8], msg: &[u8], sig: &[u8]) -> Result<(), ProofError> {
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

#[cfg(test)]
mod tests {
    //! Lock the exact enroll transcript byte layout and exercise accept/reject.
    //!
    //! Signing uses ed25519-dalek's `SigningKey` directly (the device's
    //! `DeviceKey` wraps the same primitive), so these tests validate the full
    //! sign->verify contract without depending on kallip-e2ee.

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
