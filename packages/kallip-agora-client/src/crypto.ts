// E2EE crypto for the online chat data-plane. Mirrors the Rust relay's
// `crates/platform/kallip-e2ee/src/lib.rs` (sign + AEAD half) and
// `crates/platform/kallip-agora-common/src/proof.rs` (verify half) byte-for-byte:
// X25519 ECDH + HKDF-SHA256 key derivation, ChaCha20-Poly1305 AEAD with a
// direction-tagged sequence nonce, and Ed25519 verification of the key-exchange
// transcript. The agora forwards these bytes but never decrypts, so the browser
// (initiator) and the tagma relay (responder) must agree on every detail below.
//
// Role vocabulary: the browser is the **initiator** (starts the key exchange,
// holds the per-conversation ephemeral key); the tagma is the **responder**
// (holds the long-lived Ed25519 device key, signs, derives the session key).

// Explicit `.js` suffixes are required by Deno's module resolution (Node would
// resolve the bare path). Do not strip them.
import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { ed25519, x25519 } from "@noble/curves/ed25519.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";

/** HKDF info string binding the derived key to this protocol/version.
 * WIRE-PROTOCOL: must match the Rust relay byte-for-byte. */
export const HKDF_INFO = new TextEncoder().encode("kallip-agora-aead-v1");

/** Key-exchange transcript domain-separation tag. */
export const KEX_TAG = "kallip-agora-kex-v1";

/** AEAD nonce direction tag: 0 = initiator->responder (initiator encrypts),
 * 1 = responder->initiator.
 * WIRE-PROTOCOL: must match the Rust relay byte-for-byte. */
export const DIR_INITIATOR_TO_RESPONDER = 0 as const;
export const DIR_RESPONDER_TO_INITIATOR = 1 as const;

/** The two legal AEAD direction tags; narrows the AEAD API so a bad direction
 * value is a type error, not a silent nonce mismatch. */
export type Direction =
  | typeof DIR_INITIATOR_TO_RESPONDER
  | typeof DIR_RESPONDER_TO_INITIATOR;

const TEXT_ENCODER = new TextEncoder();

/** Append a 4-byte big-endian length prefix followed by the bytes (proof.rs::framed). */
function framed(out: number[], bytes: Uint8Array): void {
  const len = bytes.length;
  out.push(
    (len >>> 24) & 0xff,
    (len >>> 16) & 0xff,
    (len >>> 8) & 0xff,
    len & 0xff,
  );
  for (let i = 0; i < bytes.length; i++) out.push(bytes[i]!);
}

/**
 * The transcript the responder signs in a key-exchange response:
 * `tag || len(tagma_id) || tagma_id || len(conv_id) || conv_id ||
 * initiator_eph[32] || responder_eph[32]`. Mirrors proof.rs::kex_transcript
 * (`kallip-agora-kex-v1`).
 */
export function kexTranscript(
  tagmaId: string,
  conversationId: string,
  initiatorEph: Uint8Array,
  responderEph: Uint8Array,
): Uint8Array {
  if (initiatorEph.length !== 32 || responderEph.length !== 32) {
    throw new Error("ephemeral keys must be 32 bytes");
  }
  const out: number[] = [];
  const tag = TEXT_ENCODER.encode(KEX_TAG);
  for (let i = 0; i < tag.length; i++) out.push(tag[i]!);
  framed(out, TEXT_ENCODER.encode(tagmaId));
  framed(out, TEXT_ENCODER.encode(conversationId));
  for (let i = 0; i < 32; i++) out.push(initiatorEph[i]!);
  for (let i = 0; i < 32; i++) out.push(responderEph[i]!);
  return new Uint8Array(out);
}

/**
 * Verify the responder's Ed25519 key-exchange signature against the pinned device
 * key. `@noble/curves`'s `verify` is strict by default (RFC 8032 §5.1.7 cofactor
 * check), matching the Rust `verify_strict`. Returns false (never throws) on
 * a malformed key or signature.
 */
export function verifyKeyExchange(
  pinnedKey: Uint8Array,
  tagmaId: string,
  conversationId: string,
  initiatorEph: Uint8Array,
  responderEph: Uint8Array,
  signature: Uint8Array,
): boolean {
  const message = kexTranscript(
    tagmaId,
    conversationId,
    initiatorEph,
    responderEph,
  );
  try {
    return ed25519.verify(signature, message, pinnedKey);
  } catch {
    return false;
  }
}

/** Generate an ephemeral X25519 keypair (32-byte private seed + public key). */
export function generateEphemeralKeyPair(): {
  privateKey: Uint8Array;
  publicKey: Uint8Array;
} {
  const privateKey = x25519.utils.randomSecretKey();
  const publicKey = x25519.getPublicKey(privateKey);
  return { privateKey, publicKey };
}

/**
 * Derive the 32-byte AEAD session key from the initiator's ephemeral private key
 * and the responder's ephemeral public key via X25519 ECDH + HKDF-SHA256 (no
 * salt; the shared secret is high-entropy). A non-contributory (all-zero) shared
 * secret — a low-order or identity peer public key — is rejected, mirroring the
 * Rust relay's `was_contributory()` check.
 */
export function deriveSessionKey(
  initiatorPrivateKey: Uint8Array,
  responderEph: Uint8Array,
): Uint8Array {
  const shared = x25519.scalarMult(initiatorPrivateKey, responderEph);
  let allZero = true;
  for (let i = 0; i < shared.length; i++) {
    if (shared[i] !== 0) {
      allZero = false;
      break;
    }
  }
  if (allZero) {
    throw new Error("non-contributory key exchange (low-order public key)");
  }
  // HKDF-SHA256 with an empty salt: equivalent to the Rust `Hkdf::new(None, ..)`
  // (HMAC pads an empty key to zeros). Single 32-byte output block.
  return hkdf(sha256, shared, new Uint8Array(), HKDF_INFO, 32);
}

/**
 * Build the 12-byte AEAD nonce: 4-byte big-endian direction || 8-byte big-endian
 * sequence. Matches e2e.rs::nonce. `dir` is 0 or 1 (the upper three bytes are
 * wire-format padding and stay zero); `seq` fits in JS's safe-integer range
 * (< 2^53), which covers the practical lifetime of a session key.
 */
function nonce(dir: Direction, seq: number): Uint8Array {
  const n = new Uint8Array(12);
  n[0] = (dir >>> 24) & 0xff;
  n[1] = (dir >>> 16) & 0xff;
  n[2] = (dir >>> 8) & 0xff;
  n[3] = dir & 0xff;
  const hi = Math.floor(seq / 0x100000000);
  const lo = seq >>> 0;
  n[4] = (hi >>> 24) & 0xff;
  n[5] = (hi >>> 16) & 0xff;
  n[6] = (hi >>> 8) & 0xff;
  n[7] = hi & 0xff;
  n[8] = (lo >>> 24) & 0xff;
  n[9] = (lo >>> 16) & 0xff;
  n[10] = (lo >>> 8) & 0xff;
  n[11] = lo & 0xff;
  return n;
}

/** Encrypt a plaintext under the given direction tag and sequence counter.
 * Returns ciphertext with the 16-byte Poly1305 tag appended. */
export function aeadEncrypt(
  key: Uint8Array,
  dir: Direction,
  seq: number,
  plaintext: Uint8Array,
): Uint8Array {
  return chacha20poly1305(key, nonce(dir, seq)).encrypt(plaintext);
}

/** Decrypt a ciphertext + tag. `null` on any AEAD failure (tampering, wrong
 * key/nonce); never throws. */
export function aeadDecrypt(
  key: Uint8Array,
  dir: Direction,
  seq: number,
  ciphertext: Uint8Array,
): Uint8Array | null {
  try {
    return chacha20poly1305(key, nonce(dir, seq)).decrypt(ciphertext);
  } catch {
    return null;
  }
}
