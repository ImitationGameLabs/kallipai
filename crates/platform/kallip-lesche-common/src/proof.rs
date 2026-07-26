//! Data-plane signed proofs: the **tunnel** reconnect proof (`GET /v1/tunnel`)
//! and the **key-exchange** proof. The shared verification primitive
//! ([`kallip_agora_common::proof::verify`]), the [`framed`] length-prefix
//! helper, and the [`ProofError`] type live in `kallip-agora-common`; the
//! enroll proof also lives there. Every variable-length field is length-prefixed
//! (4-byte big-endian) so the wire contract is unambiguous.

use kallip_agora_common::proof::{ProofError, framed, verify};

const TUNNEL_TAG: &[u8] = b"kallip-agora-tunnel-proof-v1";
const KEX_TAG: &[u8] = b"kallip-agora-kex-v1";

/// Transcript signed on every tunnel (re)connect:
/// `tag || len(responder_id) || responder_id || unix_secs(8 be)`.
pub fn tunnel_transcript(responder_id: &str, unix_secs: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(TUNNEL_TAG.len() + 4 + responder_id.len() + 8);
    out.extend_from_slice(TUNNEL_TAG);
    framed(&mut out, responder_id.as_bytes());
    out.extend_from_slice(&unix_secs.to_be_bytes());
    out
}

/// Transcript signed in a key-exchange response:
/// `tag || responder_id || conv_id || initiator_eph || responder_eph` (each
/// string length-prefixed; the 32-byte ephemeral keys are fixed-width).
pub fn kex_transcript(
    responder_id: &str,
    conv_id: &str,
    initiator_eph: &[u8],
    responder_eph: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(KEX_TAG.len() + 8 + responder_id.len() + conv_id.len() + 64);
    out.extend_from_slice(KEX_TAG);
    framed(&mut out, responder_id.as_bytes());
    framed(&mut out, conv_id.as_bytes());
    out.extend_from_slice(initiator_eph);
    out.extend_from_slice(responder_eph);
    out
}

/// Verify a tunnel reconnect proof (signature over [`tunnel_transcript`]).
/// The caller checks the timestamp skew separately.
pub fn verify_tunnel_proof(
    device_pubkey: &[u8],
    responder_id: &str,
    unix_secs: i64,
    sig: &[u8],
) -> Result<(), ProofError> {
    verify(
        device_pubkey,
        &tunnel_transcript(responder_id, unix_secs),
        sig,
    )
}

/// Verify a key-exchange proof (signature over [`kex_transcript`]).
pub fn verify_kex_proof(
    device_pubkey: &[u8],
    responder_id: &str,
    conv_id: &str,
    initiator_eph: &[u8],
    responder_eph: &[u8],
    sig: &[u8],
) -> Result<(), ProofError> {
    verify(
        device_pubkey,
        &kex_transcript(responder_id, conv_id, initiator_eph, responder_eph),
        sig,
    )
}

#[cfg(test)]
mod tests {
    //! Lock the tunnel/kex transcript byte layout and exercise accept/reject.
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
        // A responder_id ending in bytes that look like a length prefix must not
        // be re-parseable as a shorter responder_id + timestamp.
        let a = tunnel_transcript("AB", 0x4142_4344_4546_4748);
        let b = tunnel_transcript("ABCDEFGH", 0);
        assert_ne!(a, b, "length-prefixing must make transcripts unambiguous");
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
        let initiator_eph = [0xaa; 32];
        let responder_eph = [0xbb; 32];
        let sig = signing
            .sign(&kex_transcript(
                "tagma",
                "conv",
                &initiator_eph,
                &responder_eph,
            ))
            .to_bytes();

        // Happy path.
        assert!(
            verify_kex_proof(
                &public,
                "tagma",
                "conv",
                &initiator_eph,
                &responder_eph,
                &sig
            )
            .is_ok()
        );
        // Wrong conversation.
        assert!(
            verify_kex_proof(
                &public,
                "tagma",
                "other",
                &initiator_eph,
                &responder_eph,
                &sig
            )
            .is_err()
        );
        // Wrong responder.
        assert!(
            verify_kex_proof(
                &public,
                "other",
                "conv",
                &initiator_eph,
                &responder_eph,
                &sig
            )
            .is_err()
        );
        // Tampered ephemeral key.
        let mut bad_eph = initiator_eph;
        bad_eph[0] ^= 0xff;
        assert!(
            verify_kex_proof(&public, "tagma", "conv", &bad_eph, &responder_eph, &sig).is_err()
        );
        // Different responder key.
        let other_sig = SigningKey::from_bytes(&[0x99; 32])
            .sign(&kex_transcript(
                "tagma",
                "conv",
                &initiator_eph,
                &responder_eph,
            ))
            .to_bytes();
        assert!(
            verify_kex_proof(
                &public,
                "tagma",
                "conv",
                &initiator_eph,
                &responder_eph,
                &other_sig
            )
            .is_err()
        );
    }
}
