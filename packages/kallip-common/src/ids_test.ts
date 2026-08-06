// Pinned vectors for the v5 `ParticipantId` derivation. These MUST match the
// Rust derivation byte-for-byte (`ParticipantId::for_user` / `for_tagma` in
// `crates/platform/kallip-agora-common/src/ids.rs`): the lesche authenticates
// room-envelope senders against the derived id, and the relay fans room
// envelopes by it, so a TS/Rust mismatch silently breaks rooms. The expected
// values are RFC 4122 v5 over the namespace + the UTF-8 of the id string.

import { assert, assertEquals } from "@std/assert";
import { participantIdForTagma, participantIdForUser } from "./ids.ts";

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
