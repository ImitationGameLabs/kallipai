// Round-trip + known-vector coverage for the base64url codec. This is the
// highest-bug-risk piece of the WebAuthn flow (an off-by-one in padding or a
// `+`/`-` swap silently breaks every ceremony while still type-checking), so it
// gets its own test -- the first `deno test` in this package.

import { assertEquals } from "@std/assert";
import * as b64u from "./base64url.ts";

// RFC 4648 section 10 / RFC 7049 base64url test vectors, unpadded.
const VECTORS: Array<[string, number[]]> = [
  ["", []],
  ["Zg", [0x66]],
  ["Zm8", [0x66, 0x6f]],
  ["Zm9v", [0x66, 0x6f, 0x6f]],
  ["Zm9vYg", [0x66, 0x6f, 0x6f, 0x62]],
  ["Zm9vYmE", [0x66, 0x6f, 0x6f, 0x62, 0x61]],
  ["Zm9vYmFy", [0x66, 0x6f, 0x6f, 0x62, 0x61, 0x72]],
];

Deno.test("encode matches the unpadded base64url vectors", () => {
  for (const [expected, bytes] of VECTORS) {
    assertEquals(b64u.encode(new Uint8Array(bytes)), expected);
  }
});

Deno.test("decode matches the unpadded base64url vectors", () => {
  for (const [input, bytes] of VECTORS) {
    assertEquals(Array.from(b64u.decode(input)), bytes);
  }
});

Deno.test(
  "round-trips arbitrary bytes via both ArrayBuffer and Uint8Array",
  () => {
    const bytes = new Uint8Array([0, 1, 2, 254, 255, 128, 64, 32]);
    // The WebAuthn wire invariant: a credential's `id` equals the base64url of
    // its `rawId` bytes. Encode-then-decode-then-encode must be stable.
    const encoded = b64u.encode(bytes);
    assertEquals(
      b64u.encode(bytes.buffer),
      encoded,
      "ArrayBuffer input yields the same string",
    );
    assertEquals(
      b64u.encode(b64u.decode(encoded)),
      encoded,
      "round-trip is stable",
    );
  },
);

Deno.test("decode tolerates padded input", () => {
  // A server or test fixture may emit padding; decode must still recover the bytes.
  const padded = "Zm9vYg==";
  assertEquals(Array.from(b64u.decode(padded)), [0x66, 0x6f, 0x6f, 0x62]);
});
