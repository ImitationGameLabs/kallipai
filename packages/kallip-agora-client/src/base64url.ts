// Unpadded base64url (RFC 4648 section 5) encode/decode for the WebAuthn
// ceremony transforms. Browser-only: uses the Web `btoa`/`atob` globals and
// `Uint8Array` -- it MUST NOT import `node:buffer`, since the codec ships in
// the browser bundle where Node's `Buffer` is not a global.

/** Input accepted by [`encode`]: any byte buffer the Web Platform exposes. */
export type Bytes = ArrayBuffer | Uint8Array;

/** Encode bytes as an unpadded base64url `String`. */
export function encode(input: Bytes): string {
  const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
  // Build a binary string one byte at a time; `btoa` is the Web global.
  let binary = "";
  for (let i = 0; i < bytes.length; i++)
    binary += String.fromCharCode(bytes[i]!);
  return btoa(binary)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

/** Decode an unpadded base64url `String` into a fresh `Uint8Array`. */
export function decode(value: string): Uint8Array<ArrayBuffer> {
  // Restore standard base64 with padding so `atob` accepts it.
  const b64 = value.replace(/-/g, "+").replace(/_/g, "/");
  const padded = b64 + "=".repeat((4 - (b64.length % 4)) % 4);
  const binary = atob(padded);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}
