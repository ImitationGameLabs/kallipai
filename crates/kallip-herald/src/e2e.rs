//! End-to-end crypto: Ed25519 device key, X3DH-style key agreement, and
//! ChaCha20-Poly1305 AEAD. The agora forwards these messages but never holds a
//! private half, so it cannot derive the session key.

use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::Aead};
use ed25519_dalek::{Signer, SigningKey};
use hkdf::Hkdf;
use kallip_agora_common::bytes::{Ed25519Signature, X25519PublicKey};
use kallip_agora_common::control::{KeyExchangeInit, KeyExchangeResponse};
use kallip_agora_common::proof::kex_transcript;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519Public};

/// HKDF info string binding the derived key to this protocol/version.
const HKDF_INFO: &[u8] = b"kallip-agora-herald-aead-v1";

/// A 32-byte per-conversation AEAD session key.
pub type SessionKey = [u8; 32];

/// AEAD nonce direction tag: 0 = app->herald (herald decrypts), 1 = herald->app
/// (herald encrypts). The counter half is the envelope's `sequence_n`.
const DIR_APP_TO_HERALD: u32 = 0;
const DIR_HERALD_TO_APP: u32 = 1;

/// The herald's long-lived Ed25519 device key, pinned at the agora at enrollment
/// and used to sign key-exchange responses.
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

/// Respond to an app key-exchange init: generate the herald's ephemeral X25519
/// key, ECDH with the app's public, HKDF -> session key. Returns the response
/// (herald ephemeral public + signature over the transcript) and the session key.
///
/// The signature binds the ephemeral keys to `(team_id, conversation_id,
/// agent_id)` (see [`kallip_agora_common::proof::kex_transcript`]), so the app
/// can attribute the derived key unambiguously to the pinned identity.
pub fn respond_key_exchange(
    device: &DeviceKey,
    team_id: &str,
    conversation_id: &str,
    agent_id: &str,
    init: &KeyExchangeInit,
) -> anyhow::Result<(KeyExchangeResponse, SessionKey)> {
    let app_eph = array32(&init.ephemeral_public.0)?;
    // EphemeralSecret enforces single-use at compile time: `diffie_hellman`
    // consumes it. Take the public half first, then ECDH.
    let eph_secret = EphemeralSecret::random();
    let eph_pub = X25519Public::from(&eph_secret);
    let shared = eph_secret.diffie_hellman(&X25519Public::from(app_eph));
    // Reject a non-contributory (low-order/identity) peer key: otherwise an
    // attacker-chosen low-order public key forces an all-zero shared secret and
    // thus a publicly-known AEAD session key.
    if !shared.was_contributory() {
        anyhow::bail!("non-contributory key exchange (low-order public key)");
    }
    let key = hkdf_sha256_32(shared.as_bytes(), HKDF_INFO);

    let herald_eph = eph_pub.to_bytes();
    let transcript = kex_transcript(team_id, conversation_id, agent_id, &app_eph, &herald_eph);
    let signature = device.sign(&transcript);
    Ok((
        KeyExchangeResponse {
            ephemeral_public: X25519PublicKey(herald_eph.to_vec()),
            signature: Ed25519Signature(signature.to_vec()),
        },
        key,
    ))
}

/// Encrypt a herald->app plaintext (direction 1, counter = `seq`).
pub fn encrypt(key: &SessionKey, seq: u64, plaintext: &[u8]) -> Vec<u8> {
    // `SessionKey = [u8; 32]` is exactly the ChaCha20-Poly1305 key length, so
    // construction is infallible; the AEAD op itself is infallible for an
    // in-memory plaintext (it only errors on implausible buffer-length limits).
    let aead = ChaCha20Poly1305::new(key.into());
    aead.encrypt(&Nonce::from(nonce(DIR_HERALD_TO_APP, seq)), plaintext)
        .expect("chacha20poly1305 encryption is infallible for in-memory plaintext")
}

/// Decrypt an app->herald ciphertext (direction 0, counter = `seq`). `None` on
/// any AEAD failure (tampering, wrong key/nonce).
pub fn decrypt(key: &SessionKey, seq: u64, ciphertext: &[u8]) -> Option<Vec<u8>> {
    let aead = ChaCha20Poly1305::new(key.into());
    aead.decrypt(&Nonce::from(nonce(DIR_APP_TO_HERALD, seq)), ciphertext)
        .ok()
}

fn nonce(dir: u32, seq: u64) -> [u8; 12] {
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
    //! app side: both endpoints must agree on the X3DH-derived key, and AEAD
    //! must round-trip in both directions and reject tampering / wrong keys.

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

    /// App-side key derivation (mirrors the herald's HKDF step). The app holds
    /// its ephemeral across the KEX round-trip, so `ReusableSecret` (not
    /// `EphemeralSecret`) models it.
    fn app_derive_key(app_secret: &ReusableSecret, herald_eph: [u8; 32]) -> [u8; 32] {
        let shared = app_secret.diffie_hellman(&X25519Public::from(herald_eph));
        assert!(
            shared.was_contributory(),
            "test app key must be contributory"
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

    #[test]
    fn key_exchange_both_sides_agree() {
        let device = DeviceKey::generate();
        // App generates an ephemeral keypair and publishes the public half.
        let app_secret = ReusableSecret::random();
        let app_pub = X25519Public::from(&app_secret);
        let init = KeyExchangeInit {
            ephemeral_public: X25519PublicKey(app_pub.to_bytes().to_vec()),
        };
        // Herald responds, deriving its key.
        let (response, herald_key) =
            respond_key_exchange(&device, "team", "conv", "agent", &init).unwrap();
        // App independently derives the key from the herald's ephemeral public.
        let herald_eph = array32(&response.ephemeral_public.0).unwrap();
        let app_key = app_derive_key(&app_secret, herald_eph);
        assert_eq!(app_key, herald_key, "both sides must derive the same key");
    }

    #[test]
    fn kex_signature_binds_conversation_and_verifies_against_pinned_key() {
        // The app side: it knows the herald's pinned public key (fetched via
        // GET /v1/teams) and reconstructs the transcript to verify the response
        // signature, then derives the same key.
        let device = DeviceKey::generate();
        let pinned = device.public_bytes();
        let app_secret = ReusableSecret::random();
        // The transcript binds the app's PUBLIC ephemeral key (the bytes the
        // herald sees in `init.ephemeral_public`), not its private seed.
        let app_eph_pub = X25519Public::from(&app_secret).to_bytes();
        let init = KeyExchangeInit {
            ephemeral_public: X25519PublicKey(app_eph_pub.to_vec()),
        };
        let (response, _herald_key) =
            respond_key_exchange(&device, "team-7", "conv-9", "agent-3", &init).unwrap();

        let herald_eph = array32(&response.ephemeral_public.0).unwrap();
        // Verify via the shared agora-common verifier (the app SDK does this).
        assert!(
            verify_kex_proof(
                &pinned,
                "team-7",
                "conv-9",
                "agent-3",
                &app_eph_pub,
                &herald_eph,
                &response.signature.0,
            )
            .is_ok(),
            "response signature must verify against the pinned key for this binding"
        );
        // A different conversation must NOT verify (cross-wiring is closed).
        assert!(
            verify_kex_proof(
                &pinned,
                "team-7",
                "conv-OTHER",
                "agent-3",
                &app_eph_pub,
                &herald_eph,
                &response.signature.0,
            )
            .is_err()
        );
        // Belt-and-suspenders: the raw dalek verify over the same transcript
        // also passes (independent of the agora-common helper).
        let key = VerifyingKey::from_bytes(&pinned).unwrap();
        let sig = Signature::from_slice(&response.signature.0).unwrap();
        let transcript = kex_transcript("team-7", "conv-9", "agent-3", &app_eph_pub, &herald_eph);
        assert!(key.verify(&transcript, &sig).is_ok());
    }

    #[test]
    fn app_to_herald_roundtrips() {
        let key = fresh_seed();
        let plaintext = b"hello agent";
        // App encrypts (direction 0); herald's `decrypt` uses direction 0.
        let ciphertext = aead_encrypt(&key, super::DIR_APP_TO_HERALD, 7, plaintext);
        let recovered = decrypt(&key, 7, &ciphertext).expect("herald decrypts");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn herald_to_app_roundtrips() {
        let key = fresh_seed();
        let plaintext = b"reply to app";
        // Herald encrypts (direction 1); app decrypts (direction 1).
        let ciphertext = encrypt(&key, 3, plaintext);
        let recovered = aead_decrypt(&key, super::DIR_HERALD_TO_APP, 3, &ciphertext);
        assert_eq!(recovered.as_deref(), Some(plaintext.as_slice()));
    }

    #[test]
    fn direction_tags_must_differ() {
        // A ciphertext encrypted under direction 0 must NOT decrypt under direction 1
        // (the direction tag is part of the nonce).
        let key = fresh_seed();
        let ciphertext = aead_encrypt(&key, super::DIR_APP_TO_HERALD, 1, b"x");
        assert!(aead_decrypt(&key, super::DIR_HERALD_TO_APP, 1, &ciphertext).is_none());
    }

    #[test]
    fn tamper_is_rejected() {
        let key = fresh_seed();
        let mut ciphertext = aead_encrypt(&key, super::DIR_APP_TO_HERALD, 1, b"secret");
        ciphertext[0] ^= 0xff;
        assert!(decrypt(&key, 1, &ciphertext).is_none());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let key = fresh_seed();
        let other = fresh_seed();
        let ciphertext = aead_encrypt(&key, super::DIR_APP_TO_HERALD, 1, b"secret");
        assert!(decrypt(&other, 1, &ciphertext).is_none());
    }

    #[test]
    fn replayed_sequence_re_decrypts_identically() {
        // The AEAD itself does not reject a reused sequence_n (that is the
        // receiver's window job); the same (key, nonce) decrypts the same
        // ciphertext. This documents the boundary: replay protection lives in
        // the agora's seq_seen + the receiver's E2E window, not the AEAD.
        let key = fresh_seed();
        let ciphertext = aead_encrypt(&key, super::DIR_APP_TO_HERALD, 5, b"once");
        assert!(decrypt(&key, 5, &ciphertext).is_some());
        assert!(decrypt(&key, 5, &ciphertext).is_some());
    }

    #[test]
    fn low_order_public_key_is_rejected() {
        // An all-zero X25519 public key is a valid curve point but low-order:
        // the DH output is the identity for any private key, so the session key
        // would be publicly known. The herald must refuse such a key exchange
        // rather than derive a key from a non-contributory result.
        let device = DeviceKey::generate();
        let init = KeyExchangeInit {
            ephemeral_public: X25519PublicKey(vec![0u8; 32]),
        };
        let result = respond_key_exchange(&device, "team", "conv", "agent", &init);
        assert!(result.is_err(), "low-order app public key must be rejected");
    }
}
