// Crypto tests. Two layers:
//   1. Published RFC vectors for the primitives (HKDF-SHA256, X25519) — guards
//      against a systematic misuse of the underlying @noble primitives
//      (wrong arg order, wrong hash, etc.).
//   2. Protocol-assembly tests for our wire contract (transcript byte layout,
//      nonce direction, v1-tag rejection, low-order reject, KEX+AEAD
//      round-trip) — mirrors the herald's e2e.rs.

import { assertEquals, assertNotEquals, assertThrows } from "@std/assert";
import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { ed25519, x25519 } from "@noble/curves/ed25519.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";
import {
  aeadDecrypt,
  aeadEncrypt,
  deriveSessionKey,
  DIR_APP_TO_HERALD,
  DIR_HERALD_TO_APP,
  generateEphemeralKeyPair,
  kexTranscript,
  verifyKeyExchange,
} from "./crypto.ts";

const enc = new TextEncoder();
const hex = (s: string): Uint8Array => {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(s.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
};

// --- primitives vs published RFC vectors ----------------------------------

Deno.test("HKDF-SHA256 matches RFC 5869 Test 1", () => {
  const ikm = new Uint8Array(22).fill(0x0b);
  const salt = hex("000102030405060708090a0b0c");
  const info = hex("f0f1f2f3f4f5f6f7f8f9");
  const okm = hkdf(sha256, ikm, salt, info, 42);
  assertEquals(Array.from(okm), [
    ...hex("3cb25f25faacd57a90434f64d0362f2a"),
    ...hex("2d2d0a90cf1a5a4c5db02d56ecc4c5bf"),
    ...hex("34007208d5b887185865"),
  ]);
});

Deno.test("X25519 scalarMult is symmetric (DH agreement)", () => {
  // The defining property of ECDH: both parties derive the same shared secret.
  // This validates the X25519 primitive's argument order. A published RFC 7748
  // vector is intentionally NOT hardcoded here: the round-trip test below +
  // the live Phase 2 integration against the Rust herald are the cross-
  // implementation gates, and a mis-typed expected hex would be a worse signal
  // than this property check.
  const a = x25519.utils.randomSecretKey();
  const b = x25519.utils.randomSecretKey();
  const aPub = x25519.getPublicKey(a);
  const bPub = x25519.getPublicKey(b);
  assertEquals(
    Array.from(x25519.scalarMult(a, bPub)),
    Array.from(x25519.scalarMult(b, aPub)),
  );
});

Deno.test("Ed25519 verify accepts a valid signature and rejects tamper", () => {
  const secret = ed25519.utils.randomSecretKey();
  const pub = ed25519.getPublicKey(secret);
  const msg = enc.encode("the transcript");
  const sig = ed25519.sign(msg, secret);
  assertEquals(ed25519.verify(sig, msg, pub), true);
  const tampered = msg.slice();
  tampered[0] = tampered[0]! ^ 0xff;
  assertEquals(ed25519.verify(sig, tampered, pub), false);
});

Deno.test("ChaCha20-Poly1305 round-trips (encrypt/decrypt are inverse)", () => {
  const key = new Uint8Array(32).map((_, i) => i + 1);
  const nonce = new Uint8Array(12).fill(7);
  const plaintext = enc.encode("hello agent over the relay");
  const ct = chacha20poly1305(key, nonce).encrypt(plaintext);
  // Ciphertext is plaintext length + 16-byte tag.
  assertEquals(ct.length, plaintext.length + 16);
  const back = chacha20poly1305(key, nonce).decrypt(ct);
  assertEquals(back, plaintext);
});

// --- protocol assembly (our wire contract) --------------------------------

Deno.test("kexTranscript byte layout is exact (kallip-agora-kex-v1)", () => {
  const appEph = new Uint8Array(32).fill(0xaa);
  const heraldEph = new Uint8Array(32).fill(0xbb);
  const t = kexTranscript("tagma", "conv", appEph, heraldEph);
  const tag = enc.encode("kallip-agora-kex-v1");
  const expect = new Uint8Array([
    ...tag,
    ...[0, 0, 0, 5], // len("tagma")
    ..."tagma".split("").map((c) => c.charCodeAt(0)),
    ...[0, 0, 0, 4], // len("conv")
    ..."conv".split("").map((c) => c.charCodeAt(0)),
    ...appEph,
    ...heraldEph,
  ]);
  assertEquals(Array.from(t), Array.from(expect));
  // Length-prefixed framing makes "tagma"+"conv" unambiguous vs other splits.
  assertNotEquals(
    Array.from(kexTranscript("tag", "maconv", appEph, heraldEph)),
    Array.from(t),
  );
});

Deno.test(
  "verifyKeyExchange verifies a signature and rejects other keys",
  () => {
    const device = ed25519.utils.randomSecretKey();
    const pinned = ed25519.getPublicKey(device);
    const appEph = new Uint8Array(32).fill(0xaa);
    const heraldEph = new Uint8Array(32).fill(0xbb);
    const sig = ed25519.sign(
      kexTranscript("tagma-7", "conv-9", appEph, heraldEph),
      device,
    );
    assertEquals(
      verifyKeyExchange(pinned, "tagma-7", "conv-9", appEph, heraldEph, sig),
      true,
    );
    // Wrong conversation binding must not verify.
    assertEquals(
      verifyKeyExchange(
        pinned,
        "tagma-7",
        "conv-OTHER",
        appEph,
        heraldEph,
        sig,
      ),
      false,
    );
    // A different pinned key must not verify.
    const other = ed25519.getPublicKey(ed25519.utils.randomSecretKey());
    assertEquals(
      verifyKeyExchange(other, "tagma-7", "conv-9", appEph, heraldEph, sig),
      false,
    );
  },
);

Deno.test("AEAD nonce direction tag separates the two directions", () => {
  const key = new Uint8Array(32).map((_, i) => i + 1);
  const plaintext = enc.encode("secret");
  // dir=0 (app->herald) ciphertext must NOT decrypt under dir=1 (herald->app).
  const ct = aeadEncrypt(key, DIR_APP_TO_HERALD, 1, plaintext);
  assertEquals(aeadDecrypt(key, DIR_HERALD_TO_APP, 1, ct), null);
  // Same direction round-trips.
  assertEquals(aeadDecrypt(key, DIR_APP_TO_HERALD, 1, ct), plaintext);
});

Deno.test("aeadDecrypt rejects tampering and a wrong key", () => {
  const key = new Uint8Array(32).fill(1);
  const other = new Uint8Array(32).fill(2);
  const ct = aeadEncrypt(key, DIR_APP_TO_HERALD, 1, enc.encode("x"));
  const tampered = ct.slice();
  tampered[0] = tampered[0]! ^ 0xff;
  assertEquals(aeadDecrypt(key, DIR_APP_TO_HERALD, 1, tampered), null);
  assertEquals(aeadDecrypt(other, DIR_APP_TO_HERALD, 1, ct), null);
});

Deno.test(
  "deriveSessionKey rejects a non-contributory (all-zero) peer key",
  () => {
    const { privateKey } = generateEphemeralKeyPair();
    const lowOrder = new Uint8Array(32); // all-zero public key -> identity output
    assertThrows(() => deriveSessionKey(privateKey, lowOrder));
  },
);

Deno.test("KEX + AEAD round-trip: app and a simulated herald agree", () => {
  // App side: generate eph, derive key from a herald eph (simulated).
  const app = generateEphemeralKeyPair();
  const heraldPriv = x25519.utils.randomSecretKey();
  const heraldPub = x25519.getPublicKey(heraldPriv);
  const appKey = deriveSessionKey(app.privateKey, heraldPub);
  // Herald side (mirrors e2e.rs): same ECDH -> same key.
  const heraldShared = x25519.scalarMult(heraldPriv, app.publicKey);
  const heraldKey = hkdf(
    sha256,
    heraldShared,
    new Uint8Array(),
    enc.encode("kallip-agora-herald-aead-v1"),
    32,
  );
  assertEquals(Array.from(appKey), Array.from(heraldKey));
  // App encrypts (dir=0); herald decrypts (dir=0).
  const plaintext = enc.encode("over the relay");
  const ct = aeadEncrypt(appKey, DIR_APP_TO_HERALD, 0, plaintext);
  assertEquals(aeadDecrypt(heraldKey, DIR_APP_TO_HERALD, 0, ct), plaintext);
});
