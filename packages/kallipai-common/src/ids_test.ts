// Pinned vectors for the v5 `ParticipantId` derivation. These MUST match the
// Rust derivation byte-for-byte (`ParticipantId::for_user` / `for_tagma` in
// `crates/platform/kallipai-archeion-common/src/ids.rs`): the lesche authenticates
// room-envelope senders against the derived id, and the relay fans room
// envelopes by it, so a TS/Rust mismatch silently breaks rooms. The expected
// values are RFC 4122 v5 over the namespace + the UTF-8 of the id string.

import { assert, assertEquals } from "@std/assert";
import {
  participantIdForTagma,
  participantIdForUser,
  sha1,
  uuidV4,
} from "./ids.ts";

Deno.test("participantIdForUser matches the Rust v5 derivation", async () => {
  assertEquals(
    await participantIdForUser("user-1"),
    "2ff68596-16e7-5c05-9fea-986d97367b95",
  );
  assertEquals(
    await participantIdForUser("alice"),
    "6d3a23d7-4d5e-5c6d-8189-e60a82765389",
  );
});

Deno.test("participantIdForTagma matches the Rust v5 derivation", async () => {
  assertEquals(
    await participantIdForTagma("tagma-1"),
    "ee4e7dde-282f-5c2d-9592-9c809d005b09",
  );
});

Deno.test(
  "for_user and for_tagma are disjoint even on the same input",
  async () => {
    // Distinct namespaces: the same underlying string derives different ids.
    const same = "shared";
    assert(
      (await participantIdForUser(same)) !==
        (await participantIdForTagma(same)),
    );
  },
);

// RFC 3174 vector pins the hash layer beneath the v5 bit-twiddling, on
// both implementations: subtle (secure contexts) and the jssha fallback.
Deno.test("sha1 matches the RFC 3174 vector on both paths", async () => {
  const hex = (bytes: Uint8Array) =>
    Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  const abc = new TextEncoder().encode("abc");
  const expected = "a9993e364706816aba3e25717850c26c9cd0d89d";
  assertEquals(hex(await sha1(abc, crypto.subtle)), expected);
  assertEquals(hex(await sha1(abc, undefined)), expected);
});

// The fallback exists because subtle is undefined off secure contexts;
// the paths must agree bit-for-bit or the v5 ids drift between shapes.
Deno.test("sha1 subtle and jssha paths agree bit-for-bit", async () => {
  const encoder = new TextEncoder();
  const inputs = [
    encoder.encode(""),
    new Uint8Array(37), // the v5 shape: 16 namespace bytes + a name
    encoder.encode("kallipai.lan whoami"),
  ];
  for (const data of inputs) {
    assertEquals(await sha1(data, undefined), await sha1(data, crypto.subtle));
  }
});

// crypto.randomUUID is secure-context-only; the fallback assembles a v4
// from getRandomValues, which every context exposes.
Deno.test("uuidV4 falls back to a v4-shaped unique id", () => {
  const fallback = uuidV4(undefined);
  assert(
    /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
      fallback,
    ),
  );
  assert(fallback !== uuidV4(undefined));
});

Deno.test("uuidV4 uses the native randomUUID when present", () => {
  assertEquals(
    uuidV4(() => "native-uuid"),
    "native-uuid",
  );
});
