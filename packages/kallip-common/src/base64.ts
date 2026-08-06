// Standard base64 (RFC 4648 section 4) encode/decode, with padding and the
// `+`/`/` alphabet. The shared wire codec: the agora serializes opaque byte
// fields (ciphertext, public keys, signatures) via `general_purpose::STANDARD`
// (crates/platform/kallip-agora-common/src/bytes.rs), and the room surface
// carries envelope payloads as the same standard base64. Lives here in the
// transport-agnostic shared layer so every browser package imports one codec,
// not a per-package copy.
//
// Distinct from the WebAuthn ceremony's unpadded base64url, which is local to
// the agora client (it pairs with the WebAuthn `id`/`rawId` convention).
//
// Browser-only: uses the Web `btoa`/`atob` globals and `Uint8Array`. It MUST
// NOT import `node:buffer`, since the codec ships in the browser bundle where
// Node's `Buffer` is not a global.

/** Input accepted by [`encodeB64`]: any byte buffer the Web Platform exposes. */
export type Bytes = ArrayBuffer | Uint8Array;

/** Encode bytes as a standard, padded base64 `String`. */
export function encodeB64(input: Bytes): string {
  const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
  // Build a binary string one byte at a time; `btoa` is the Web global.
  let binary = "";
  for (let i = 0; i < bytes.length; i++)
    binary += String.fromCharCode(bytes[i]!);
  return btoa(binary);
}

/** Decode a standard, padded base64 `String` into a fresh `Uint8Array`. */
export function decodeB64(value: string): Uint8Array<ArrayBuffer> {
  // `atob` accepts standard base64 (with or without padding); the wire always
  // sends padding, but tolerate a missing pad so a hand-edited value still works.
  const binary = atob(value);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}
