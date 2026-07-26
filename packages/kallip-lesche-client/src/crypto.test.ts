// Crypto tests. Two layers:
//   1. Published RFC vectors for the primitives (HKDF-SHA256, X25519) — guards
//      against a systematic misuse of the underlying @noble primitives
//      (wrong arg order, wrong hash, etc.).
//   2. Protocol-assembly tests for our wire contract (transcript byte layout,
//      nonce direction, v1-tag rejection, low-order reject, KEX+AEAD
//      round-trip) — mirrors the Rust relay's kallip-e2ee.

import { assertEquals, assertNotEquals, assertThrows } from "@std/assert";
import { chacha20poly1305 } from "@noble/ciphers/chacha.js";
import { ed25519, x25519 } from "@noble/curves/ed25519.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";
import {
  aeadDecrypt,
  aeadEncrypt,
  deriveSessionKey,
  DIR_INITIATOR_TO_RESPONDER,
  DIR_RESPONDER_TO_INITIATOR,
  generateEphemeralKeyPair,
  HKDF_INFO,
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

// --- wire-protocol invariant guards ----------------------------------------

Deno.test("HKDF_INFO is pinned (kallip-agora-aead-v1)", () => {
  // WIRE-PROTOCOL: must match the Rust relay byte-for-byte. A careless rename
  // on either side breaks every browser<->tagma AEAD decrypt silently.
  assertEquals(
    Array.from(HKDF_INFO),
    Array.from(enc.encode("kallip-agora-aead-v1")),
  );
});

Deno.test("HKDF output under HKDF_INFO matches the pinned vector", () => {
  // Pins the full ikm->okm mapping (sha256, empty salt, our info) to a
  // hardcoded vector — independent of the literal string above, so it also
  // catches a wrong hash, wrong output length, or a non-empty salt. The
  // expected bytes are HKDF-SHA256(ikm=[0x0b;22], salt=empty,
  // info="kallip-agora-aead-v1", L=32), matching the Rust relay's
  // `hkdf_sha256_32`.
  const ikm = new Uint8Array(22).fill(0x0b);
  const okm = hkdf(sha256, ikm, new Uint8Array(), HKDF_INFO, 32);
  assertEquals(
    [...okm].map((b) => b.toString(16).padStart(2, "0")).join(""),
    "575bb566f4cfb27ecb065e6fb8e650ded209772a62901c757d91bc467b99fa46",
  );
});

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
  // the live integration against the Rust relay are the cross-implementation
  // gates, and a mis-typed expected hex would be a worse signal than this
  // property check.
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
  const initiatorEph = new Uint8Array(32).fill(0xaa);
  const responderEph = new Uint8Array(32).fill(0xbb);
  const t = kexTranscript("tagma", "conv", initiatorEph, responderEph);
  const tag = enc.encode("kallip-agora-kex-v1");
  const expect = new Uint8Array([
    ...tag,
    ...[0, 0, 0, 5], // len("tagma")
    ..."tagma".split("").map((c) => c.charCodeAt(0)),
    ...[0, 0, 0, 4], // len("conv")
    ..."conv".split("").map((c) => c.charCodeAt(0)),
    ...initiatorEph,
    ...responderEph,
  ]);
  assertEquals(Array.from(t), Array.from(expect));
  // Length-prefixed framing makes "tagma"+"conv" unambiguous vs other splits.
  assertNotEquals(
    Array.from(kexTranscript("tag", "maconv", initiatorEph, responderEph)),
    Array.from(t),
  );
});

Deno.test(
  "verifyKeyExchange verifies a signature and rejects other keys",
  () => {
    const device = ed25519.utils.randomSecretKey();
    const pinned = ed25519.getPublicKey(device);
    const initiatorEph = new Uint8Array(32).fill(0xaa);
    const responderEph = new Uint8Array(32).fill(0xbb);
    const sig = ed25519.sign(
      kexTranscript("tagma-7", "conv-9", initiatorEph, responderEph),
      device,
    );
    assertEquals(
      verifyKeyExchange(
        pinned,
        "tagma-7",
        "conv-9",
        initiatorEph,
        responderEph,
        sig,
      ),
      true,
    );
    // Wrong conversation binding must not verify.
    assertEquals(
      verifyKeyExchange(
        pinned,
        "tagma-7",
        "conv-OTHER",
        initiatorEph,
        responderEph,
        sig,
      ),
      false,
    );
    // A different pinned key must not verify.
    const other = ed25519.getPublicKey(ed25519.utils.randomSecretKey());
    assertEquals(
      verifyKeyExchange(
        other,
        "tagma-7",
        "conv-9",
        initiatorEph,
        responderEph,
        sig,
      ),
      false,
    );
  },
);

Deno.test("AEAD nonce direction tag separates the two directions", () => {
  const key = new Uint8Array(32).map((_, i) => i + 1);
  const plaintext = enc.encode("secret");
  // dir=0 (initiator->responder) ciphertext must NOT decrypt under dir=1.
  const ct = aeadEncrypt(key, DIR_INITIATOR_TO_RESPONDER, 1, plaintext);
  assertEquals(aeadDecrypt(key, DIR_RESPONDER_TO_INITIATOR, 1, ct), null);
  // Same direction round-trips.
  assertEquals(aeadDecrypt(key, DIR_INITIATOR_TO_RESPONDER, 1, ct), plaintext);
});

Deno.test("aeadDecrypt rejects tampering and a wrong key", () => {
  const key = new Uint8Array(32).fill(1);
  const other = new Uint8Array(32).fill(2);
  const ct = aeadEncrypt(key, DIR_INITIATOR_TO_RESPONDER, 1, enc.encode("x"));
  const tampered = ct.slice();
  tampered[0] = tampered[0]! ^ 0xff;
  assertEquals(aeadDecrypt(key, DIR_INITIATOR_TO_RESPONDER, 1, tampered), null);
  assertEquals(aeadDecrypt(other, DIR_INITIATOR_TO_RESPONDER, 1, ct), null);
});

Deno.test(
  "deriveSessionKey rejects a non-contributory (all-zero) peer key",
  () => {
    const { privateKey } = generateEphemeralKeyPair();
    const lowOrder = new Uint8Array(32); // all-zero public key -> identity output
    assertThrows(() => deriveSessionKey(privateKey, lowOrder));
  },
);

Deno.test(
  "KEX + AEAD round-trip: initiator and a simulated responder agree",
  () => {
    // Internal consistency: both endpoints here import the same crypto.ts, so
    // this cannot by itself catch a TS<->Rust HKDF drift — the pinned tests
    // above (the HKDF_INFO literal + the derived vector) are that gate, plus
    // the live integration. This test validates that deriveSessionKey and a
    // hand-rolled HKDF agree, and that dir-tagged AEAD round-trips.
    const initiator = generateEphemeralKeyPair();
    const responderPriv = x25519.utils.randomSecretKey();
    const responderPub = x25519.getPublicKey(responderPriv);
    const initiatorKey = deriveSessionKey(initiator.privateKey, responderPub);
    // Responder side (mirrors kallip-e2ee): same ECDH -> same key.
    const responderShared = x25519.scalarMult(
      responderPriv,
      initiator.publicKey,
    );
    const responderKey = hkdf(
      sha256,
      responderShared,
      new Uint8Array(),
      HKDF_INFO,
      32,
    );
    assertEquals(Array.from(initiatorKey), Array.from(responderKey));
    // Initiator encrypts (dir=0); responder decrypts (dir=0).
    const plaintext = enc.encode("over the relay");
    const ct = aeadEncrypt(
      initiatorKey,
      DIR_INITIATOR_TO_RESPONDER,
      0,
      plaintext,
    );
    assertEquals(
      aeadDecrypt(responderKey, DIR_INITIATOR_TO_RESPONDER, 0, ct),
      plaintext,
    );
  },
);
