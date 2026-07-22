// Standard-base64 codec tests. The wire alphabet (STANDARD, padded, +//) must
// match the agora's `bytes.rs` (`general_purpose::STANDARD`).

import { assertEquals } from "@std/assert";
import { decodeB64, encodeB64 } from "./base64.ts";

Deno.test("encodeB64 matches bytes.rs STANDARD alphabet", () => {
  // Mirrors crates/kallip-agora-common/src/bytes.rs::ciphertext_round_trips.
  assertEquals(encodeB64(Uint8Array.of(0xde, 0xad, 0xbe, 0xef)), "3q2+7w==");
});

Deno.test("encodeB64 / decodeB64 round-trip", () => {
  const cases = [
    new Uint8Array(),
    Uint8Array.of(0x00),
    Uint8Array.of(0xff, 0x01, 0x02),
    new Uint8Array(255).map((_, i) => i),
  ];
  for (const bytes of cases) {
    assertEquals(decodeB64(encodeB64(bytes)), bytes);
  }
});

Deno.test("decodeB64 tolerates missing padding", () => {
  // The wire always sends padding, but a hand-edited value may omit it; `atob`
  // accepts both. 0xdeadbeef -> "3q2+7w==" padded, "3q2+7w" unpadded.
  assertEquals(decodeB64("3q2+7w"), Uint8Array.of(0xde, 0xad, 0xbe, 0xef));
});

Deno.test("encodeB64 uses the standard + and / chars (not url-safe)", () => {
  // 0xfb 0xfc 0xfd -> contains both + (0x3e boundary) and / chars.
  const s = encodeB64(Uint8Array.of(0xfb, 0xfc, 0xfd, 0xfe, 0xff));
  if (!s.includes("+") && !s.includes("/")) {
    throw new Error(`expected STANDARD base64 (with + or /), got ${s}`);
  }
});
