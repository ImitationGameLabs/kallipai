//! End-to-end encryption primitives: an Ed25519 device key, an X3DH-style key
//! agreement, and ChaCha20-Poly1305 AEAD.
//!
//! This is the signing/AEAD (private-key) counterpart to the verify-only
//! [`kallip_agora_common::proof`] module. Together the two halves define the E2E
//! contract: a holder of this crate can sign and encrypt; a holder of only the
//! verify half can authenticate ciphertext without ever touching a private key.
//!
//! The two protocol roles are the **initiator** (the party that starts the key
//! exchange, holding a per-conversation ephemeral key) and the **responder**
//! (the party that holds the long-lived Ed25519 device key, signs, and derives
//! the session key). This crate is role-based and intentionally agnostic of
//! which service deploys the responder.
//!
//! # Invariant
//!
//! This crate holds **private-key material**. It MUST NOT become a dependency of
//! any server or verifier process — those consume only the verify half. The
//! split is precisely what lets a forwarding relay stay cryptographically blind.
//!
//! # Wire protocol
//!
//! `HKDF_INFO`, the direction tags, and the nonce layout are wire-protocol
//! constants that every endpoint must match byte-for-byte. They are pinned by
//! the regression tests at the bottom of this file; never re-version them.

use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::Aead};
use ed25519_dalek::{Signer, SigningKey};
use hkdf::Hkdf;
use kallip_agora_common::bytes::{Ed25519Signature, X25519PublicKey};
use kallip_agora_common::control::{KeyExchangeInit, KeyExchangeResponse};
use kallip_agora_common::proof::kex_transcript;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519Public};
use zeroize::Zeroize;

/// HKDF info string binding the derived key to this protocol/version.
///
/// WIRE-PROTOCOL: must match the app SDK byte-for-byte. Do not re-version.
pub const HKDF_INFO: &[u8] = b"kallip-agora-aead-v1";

/// A 32-byte per-conversation AEAD session key.
///
/// This is secret material, so the type owns its hygiene contract: it is
/// zeroized on drop (every local, every struct field, every epoch rotation —
/// set-and-forget), it is not `Copy` (forcing every extraction to be an
/// explicit borrow or clone, so no silent unzeroized copy survives), and its
/// `Debug` representation is redacted so it can never leak into a log. It
/// derefs to `[u8; 32]`, so `encrypt`/`decrypt` take `&[u8; 32]` and accept
/// either a raw array or a `&SessionKey` via deref coercion.
///
/// Note: `DeviceKey`'s Ed25519 seed lives in `ed25519-dalek`'s `SigningKey`,
/// whose drop hygiene is a separate concern (its `zeroize` feature) and is not
/// covered here.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionKey([u8; 32]);

impl SessionKey {
    /// Wrap a derived key. Takes ownership of the bytes so the caller's copy
    /// can be zeroed independently if desired.
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl std::ops::Deref for SessionKey {
    type Target = [u8; 32];

    fn deref(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for SessionKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKey([redacted])")
    }
}

/// AEAD nonce direction tag: 0 = initiator->responder (the responder decrypts),
/// 1 = responder->initiator (the responder encrypts). The counter half is the
/// envelope's `sequence_n`.
///
/// WIRE-PROTOCOL: must match the app SDK byte-for-byte.
pub const DIR_INITIATOR_TO_RESPONDER: u32 = 0;

/// WIRE-PROTOCOL: must match the app SDK byte-for-byte.
pub const DIR_RESPONDER_TO_INITIATOR: u32 = 1;

/// The long-lived Ed25519 device key, pinned at the agora at enrollment and used
/// to sign key-exchange responses.
pub struct DeviceKey {
    signing: SigningKey,
}

impl DeviceKey {
    pub fn generate() -> Self {
        Self::from_seed(fresh_seed())
    }
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(&seed),
        }
    }
    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }
    pub fn public_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }
    /// Sign an arbitrary message with the device key (used for the enroll proof,
    /// the tunnel reconnect proof, and the key-exchange transcript).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }
}

/// Respond to an initiator's key-exchange init: generate the responder's
/// ephemeral X25519 key, ECDH with the initiator's public, HKDF -> session key.
/// Returns the response (responder ephemeral public + signature over the
/// transcript) and the session key.
///
/// The signature binds the ephemeral keys to `(responder_id, conversation_id)`
/// (see [`kallip_agora_common::proof::kex_transcript`]), so the initiator can
/// attribute the derived key unambiguously to the pinned identity. The agent
/// backing the conversation is a responder-internal concern and is not part of
/// the transcript.
pub fn respond_key_exchange(
    device: &DeviceKey,
    responder_id: &str,
    conversation_id: &str,
    init: &KeyExchangeInit,
) -> anyhow::Result<(KeyExchangeResponse, SessionKey)> {
    let initiator_eph = array32(&init.ephemeral_public.0)?;
    // EphemeralSecret enforces single-use at compile time: `diffie_hellman`
    // consumes it. Take the public half first, then ECDH.
    let eph_secret = EphemeralSecret::random();
    let eph_pub = X25519Public::from(&eph_secret);
    let shared = eph_secret.diffie_hellman(&X25519Public::from(initiator_eph));
    // Reject a non-contributory (low-order/identity) peer key: otherwise an
    // attacker-chosen low-order public key forces an all-zero shared secret and
    // thus a publicly-known AEAD session key.
    if !shared.was_contributory() {
        anyhow::bail!("non-contributory key exchange (low-order public key)");
    }
    let key = hkdf_sha256_32(shared.as_bytes(), HKDF_INFO);

    let responder_eph = eph_pub.to_bytes();
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
        SessionKey::new(key),
    ))
}

/// Encrypt a responder->initiator plaintext (direction 1, counter = `seq`).
/// `key` is `[u8; 32]` (exactly the ChaCha20-Poly1305 key length), so a
/// `&SessionKey` derefs into it at the call site.
pub fn encrypt(key: &[u8; 32], seq: u64, plaintext: &[u8]) -> Vec<u8> {
    // Construction is infallible at this key length; the AEAD op itself is
    // infallible for an in-memory plaintext (it only errors on implausible
    // buffer-length limits).
    let aead = ChaCha20Poly1305::new(key.into());
    aead.encrypt(
        &Nonce::from(nonce(DIR_RESPONDER_TO_INITIATOR, seq)),
        plaintext,
    )
    .expect("chacha20poly1305 encryption is infallible for in-memory plaintext")
}

/// Decrypt an initiator->responder ciphertext (direction 0, counter = `seq`).
/// `None` on any AEAD failure (tampering, wrong key/nonce).
pub fn decrypt(key: &[u8; 32], seq: u64, ciphertext: &[u8]) -> Option<Vec<u8>> {
    let aead = ChaCha20Poly1305::new(key.into());
    aead.decrypt(
        &Nonce::from(nonce(DIR_INITIATOR_TO_RESPONDER, seq)),
        ciphertext,
    )
    .ok()
}

/// Build the 12-byte AEAD nonce: 4-byte big-endian direction tag ||
/// 8-byte big-endian sequence counter.
///
/// WIRE-PROTOCOL: must match the app SDK byte-for-byte.
pub fn nonce(dir: u32, seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0..4].copy_from_slice(&dir.to_be_bytes());
    n[4..12].copy_from_slice(&seq.to_be_bytes());
    n
}

fn fresh_seed() -> [u8; 32] {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("getrandom");
    seed
}

fn array32(v: &[u8]) -> anyhow::Result<[u8; 32]> {
    v.try_into()
        .map_err(|_| anyhow::anyhow!("expected a 32-byte X25519 public key"))
}

// --- HKDF-SHA256 (RFC 5869), single 32-byte output block ---

/// Derive a 32-byte AEAD session key from an X25519 shared secret via
/// HKDF-SHA256 (no salt; the shared secret is high-entropy). Backed by the
/// audited `hkdf` crate.
fn hkdf_sha256_32(ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, ikm);
    let mut okm = [0u8; 32];
    hk.expand(info, &mut okm)
        .expect("32 bytes is within the HKDF single-block limit");
    okm
}

#[cfg(test)]
mod tests {
    //! Validate the full E2E crypto contract at the unit level, simulating the
    //! initiator side: both endpoints must agree on the X3DH-derived key, and
    //! AEAD must round-trip in both directions and reject tampering / wrong keys.

    use super::nonce;
    use super::{
        DeviceKey, HKDF_INFO, array32, decrypt, encrypt, fresh_seed, hkdf_sha256_32,
        respond_key_exchange,
    };
    use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::Aead};
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    use kallip_agora_common::bytes::X25519PublicKey;
    use kallip_agora_common::control::KeyExchangeInit;
    use kallip_agora_common::proof::{kex_transcript, verify_kex_proof};
    use x25519_dalek::{PublicKey as X25519Public, ReusableSecret};

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
        hkdf_sha256_32(shared.as_bytes(), HKDF_INFO)
    }

    /// Encrypt with an explicit direction (simulates either endpoint).
    fn aead_encrypt(key: &[u8; 32], dir: u32, seq: u64, plaintext: &[u8]) -> Vec<u8> {
        let aead = ChaCha20Poly1305::new_from_slice(key).unwrap();
        aead.encrypt(&Nonce::from(nonce(dir, seq)), plaintext)
            .unwrap()
    }

    /// Decrypt with an explicit direction (simulates either endpoint).
    fn aead_decrypt(key: &[u8; 32], dir: u32, seq: u64, ciphertext: &[u8]) -> Option<Vec<u8>> {
        let aead = ChaCha20Poly1305::new_from_slice(key).unwrap();
        aead.decrypt(&Nonce::from(nonce(dir, seq)), ciphertext).ok()
    }

    // --- WIRE-PROTOCOL invariant guards: a careless edit fails loudly. ---

    #[test]
    fn hkdf_info_is_pinned() {
        assert_eq!(HKDF_INFO, b"kallip-agora-aead-v1");
    }

    #[test]
    fn direction_tags_are_pinned() {
        assert_eq!(super::DIR_INITIATOR_TO_RESPONDER, 0);
        assert_eq!(super::DIR_RESPONDER_TO_INITIATOR, 1);
    }

    #[test]
    fn nonce_layout_is_pinned() {
        // 4-byte BE direction || 8-byte BE sequence.
        assert_eq!(nonce(0, 0), [0u8; 12]);
        assert_eq!(
            nonce(super::DIR_RESPONDER_TO_INITIATOR, 1),
            [
                0, 0, 0, 1, // dir = 1
                0, 0, 0, 0, 0, 0, 0, 1, // seq = 1
            ]
        );
        assert_eq!(
            nonce(super::DIR_INITIATOR_TO_RESPONDER, 0xffff_ffff_ffff_ffff),
            [
                0, 0, 0, 0, // dir = 0
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            ]
        );
    }

    #[test]
    fn key_exchange_both_sides_agree() {
        let device = DeviceKey::generate();
        // The initiator generates an ephemeral keypair and publishes the public half.
        let initiator_secret = ReusableSecret::random();
        let initiator_pub = X25519Public::from(&initiator_secret);
        let init = KeyExchangeInit {
            ephemeral_public: X25519PublicKey(initiator_pub.to_bytes().to_vec()),
        };
        // The responder responds, deriving its key.
        let (response, responder_key) =
            respond_key_exchange(&device, "tagma", "conv", &init).unwrap();
        // The initiator independently derives the key from the responder's
        // ephemeral public.
        let responder_eph = array32(&response.ephemeral_public.0).unwrap();
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

        let responder_eph = array32(&response.ephemeral_public.0).unwrap();
        // Verify via the shared agora-common verifier (the app SDK does this).
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
        // Belt-and-suspenders: the raw dalek verify over the same transcript
        // also passes (independent of the agora-common helper).
        let key = VerifyingKey::from_bytes(&pinned).unwrap();
        let sig = Signature::from_slice(&response.signature.0).unwrap();
        let transcript = kex_transcript("tagma-7", "conv-9", &initiator_eph_pub, &responder_eph);
        assert!(key.verify(&transcript, &sig).is_ok());
    }

    #[test]
    fn initiator_to_responder_roundtrips() {
        let key = fresh_seed();
        let plaintext = b"hello agent";
        // The initiator encrypts (direction 0); the responder's `decrypt` uses
        // direction 0.
        let ciphertext = aead_encrypt(&key, super::DIR_INITIATOR_TO_RESPONDER, 7, plaintext);
        let recovered = decrypt(&key, 7, &ciphertext).expect("responder decrypts");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn responder_to_initiator_roundtrips() {
        let key = fresh_seed();
        let plaintext = b"reply to app";
        // The responder encrypts (direction 1); the initiator decrypts
        // (direction 1).
        let ciphertext = encrypt(&key, 3, plaintext);
        let recovered = aead_decrypt(&key, super::DIR_RESPONDER_TO_INITIATOR, 3, &ciphertext);
        assert_eq!(recovered.as_deref(), Some(plaintext.as_slice()));
    }

    #[test]
    fn direction_tags_must_differ() {
        // A ciphertext encrypted under direction 0 must NOT decrypt under direction 1
        // (the direction tag is part of the nonce).
        let key = fresh_seed();
        let ciphertext = aead_encrypt(&key, super::DIR_INITIATOR_TO_RESPONDER, 1, b"x");
        assert!(aead_decrypt(&key, super::DIR_RESPONDER_TO_INITIATOR, 1, &ciphertext).is_none());
    }

    #[test]
    fn tamper_is_rejected() {
        let key = fresh_seed();
        let mut ciphertext = aead_encrypt(&key, super::DIR_INITIATOR_TO_RESPONDER, 1, b"secret");
        ciphertext[0] ^= 0xff;
        assert!(decrypt(&key, 1, &ciphertext).is_none());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let key = fresh_seed();
        let other = fresh_seed();
        let ciphertext = aead_encrypt(&key, super::DIR_INITIATOR_TO_RESPONDER, 1, b"secret");
        assert!(decrypt(&other, 1, &ciphertext).is_none());
    }

    #[test]
    fn replayed_sequence_re_decrypts_identically() {
        // The AEAD itself does not reject a reused sequence_n (that is the
        // receiver's window job); the same (key, nonce) decrypts the same
        // ciphertext. This documents the boundary: within-epoch replay
        // protection lives in the responder's `seen_inbound` window and cross-
        // epoch in AEAD key rotation -- never in the AEAD itself, and the relay
        // (lesche) does no dedup.
        let key = fresh_seed();
        let ciphertext = aead_encrypt(&key, super::DIR_INITIATOR_TO_RESPONDER, 5, b"once");
        assert!(decrypt(&key, 5, &ciphertext).is_some());
        assert!(decrypt(&key, 5, &ciphertext).is_some());
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
