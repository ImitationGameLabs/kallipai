//! Responder-side key-exchange composition: combine the `kallip-e2ee` session-key
//! primitive with the wire transcript + device signature to produce a
//! `KeyExchangeResponse`. This is the tagma's responder business logic (it holds
//! the long-lived `DeviceKey`); the pure crypto lives in `kallip-e2ee`.

use anyhow::{Context, Result};
use kallip_agora_common::bytes::{Ed25519Signature, X25519PublicKey};
use kallip_e2ee::{DeviceKey, SessionKey, derive_responder_session_key};
use kallip_lesche_common::control::{KeyExchangeInit, KeyExchangeResponse};
use kallip_lesche_common::proof::kex_transcript;

/// Build a key-exchange response for an initiator's `init`: derive the session
/// key (ECDH + HKDF, rejecting a non-contributory peer), sign the transcript
/// binding the ephemeral keys to `(responder_id, conversation_id)`, and wrap the
/// result in the wire types. Returns the response to POST back plus the session
/// key to install locally.
pub(crate) fn respond_key_exchange(
    device: &DeviceKey,
    responder_id: &str,
    conversation_id: &str,
    init: &KeyExchangeInit,
) -> Result<(KeyExchangeResponse, SessionKey)> {
    let initiator_eph = <[u8; 32]>::try_from(init.ephemeral_public.0.as_slice())
        .context("expected a 32-byte X25519 public key")?;
    let (responder_eph, key) = derive_responder_session_key(initiator_eph)?;
    let transcript = kex_transcript(
        responder_id,
        conversation_id,
        &initiator_eph,
        &responder_eph,
    );
    let signature = device.sign(&transcript);
    Ok((
        KeyExchangeResponse {
            ephemeral_public: X25519PublicKey(responder_eph.to_vec()),
            signature: Ed25519Signature(signature.to_vec()),
        },
        key,
    ))
}

#[cfg(test)]
mod tests {
    use super::respond_key_exchange;
    use kallip_agora_common::bytes::X25519PublicKey;
    use kallip_e2ee::{DeviceKey, HKDF_INFO};
    use kallip_lesche_common::control::KeyExchangeInit;
    use kallip_lesche_common::proof::{kex_transcript, verify_kex_proof};
    use x25519_dalek::{PublicKey as X25519Public, ReusableSecret};

    use hkdf::Hkdf;
    use sha2::Sha256;

    /// Initiator-side key derivation (mirrors the responder's HKDF step). The
    /// initiator holds its ephemeral across the KEX round-trip, so
    /// `ReusableSecret` (not `EphemeralSecret`) models it.
    fn initiator_derive_key(
        initiator_secret: &ReusableSecret,
        responder_eph: [u8; 32],
    ) -> [u8; 32] {
        let shared = initiator_secret.diffie_hellman(&X25519Public::from(responder_eph));
        assert!(
            shared.was_contributory(),
            "test initiator key must be contributory"
        );
        let hk = Hkdf::<Sha256>::new(None, shared.as_bytes());
        let mut okm = [0u8; 32];
        hk.expand(HKDF_INFO, &mut okm).expect("32-byte HKDF output");
        okm
    }

    #[test]
    fn key_exchange_both_sides_agree() {
        let device = DeviceKey::generate();
        // The initiator generates an ephemeral keypair and publishes the public half.
        let initiator_secret = ReusableSecret::random();
        let initiator_pub = X25519Public::from(&initiator_secret).to_bytes();
        let init = KeyExchangeInit {
            ephemeral_public: X25519PublicKey(initiator_pub.to_vec()),
        };
        // The responder responds, deriving its key.
        let (response, responder_key) =
            respond_key_exchange(&device, "tagma", "conv", &init).unwrap();
        // The initiator independently derives the key from the responder's
        // ephemeral public.
        let responder_eph = response.ephemeral_public.0.as_slice().try_into().unwrap();
        let initiator_key = initiator_derive_key(&initiator_secret, responder_eph);
        assert_eq!(
            initiator_key, *responder_key,
            "both sides must derive the same key"
        );
    }

    #[test]
    fn kex_signature_binds_conversation_and_verifies_against_pinned_key() {
        // The initiator side: it knows the responder's pinned public key
        // (fetched via GET /v1/tagmata) and reconstructs the transcript to
        // verify the response signature, then derives the same key.
        let device = DeviceKey::generate();
        let pinned = device.public_bytes();
        let initiator_secret = ReusableSecret::random();
        // The transcript binds the initiator's PUBLIC ephemeral key (the bytes
        // the responder sees in `init.ephemeral_public`), not its private seed.
        let initiator_eph_pub = X25519Public::from(&initiator_secret).to_bytes();
        let init = KeyExchangeInit {
            ephemeral_public: X25519PublicKey(initiator_eph_pub.to_vec()),
        };
        let (response, _responder_key) =
            respond_key_exchange(&device, "tagma-7", "conv-9", &init).unwrap();

        let responder_eph: [u8; 32] = response.ephemeral_public.0.as_slice().try_into().unwrap();
        // Verify via the shared lesche-common verifier (the app SDK does this).
        assert!(
            verify_kex_proof(
                &pinned,
                "tagma-7",
                "conv-9",
                &initiator_eph_pub,
                &responder_eph,
                &response.signature.0,
            )
            .is_ok(),
            "response signature must verify against the pinned key for this binding"
        );
        // A different conversation must NOT verify (cross-wiring is closed).
        assert!(
            verify_kex_proof(
                &pinned,
                "tagma-7",
                "conv-OTHER",
                &initiator_eph_pub,
                &responder_eph,
                &response.signature.0,
            )
            .is_err()
        );
        // Belt-and-suspenders: the transcript the signature covers is exactly
        // `kex_transcript(...)` -- the same builder the verifier reconstructs.
        let transcript = kex_transcript("tagma-7", "conv-9", &initiator_eph_pub, &responder_eph);
        assert!(
            verify_kex_proof(
                &pinned,
                "tagma-7",
                "conv-9",
                &initiator_eph_pub,
                &responder_eph,
                &device.sign(&transcript),
            )
            .is_ok(),
            "a direct device signature over the transcript must verify"
        );
    }

    #[test]
    fn low_order_public_key_is_rejected() {
        // An all-zero X25519 public key is a valid curve point but low-order:
        // the DH output is the identity for any private key, so the session key
        // would be publicly known. The responder must refuse such a key exchange
        // rather than derive a key from a non-contributory result.
        let device = DeviceKey::generate();
        let init = KeyExchangeInit {
            ephemeral_public: X25519PublicKey(vec![0u8; 32]),
        };
        let result = respond_key_exchange(&device, "tagma", "conv", &init);
        assert!(
            result.is_err(),
            "low-order initiator public key must be rejected"
        );
    }
}
