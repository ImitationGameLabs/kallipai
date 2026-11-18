// Behavior test for the composer's state mapping: one sentence covers every way
// a write axis can die, and only a relay channel offers a Retry -- it is the one
// carrier the store can be asked to reopen. Each case is a branch of the table
// the chat page feeds, so deleting a branch turns one of these red.

import { assertEquals } from "@std/assert";
import { composerState, noticeRender } from "./composerState.ts";

Deno.test("composerState announces the handshake", () => {
  assertEquals(
    composerState({ kind: "relay", status: "opening", canWrite: false }),
    { notice: "opening", retry: false },
  );
});

Deno.test(
  "composerState announces the handshake ahead of a writable axis",
  () => {
    assertEquals(
      composerState({ kind: "relay", status: "opening", canWrite: true }),
      { notice: "opening", retry: false },
    );
  },
);

Deno.test("composerState stays silent while the axis can write", () => {
  const writable: Array<Parameters<typeof composerState>[0]> = [
    { kind: "relay", status: "open", canWrite: true },
    { kind: "local", status: "reconnecting", canWrite: true },
    { kind: "offline-view", status: "offline", canWrite: true },
  ];
  for (const c of writable) {
    assertEquals(composerState(c), { notice: null, retry: false }, c.kind);
  }
});

Deno.test(
  "composerState says not connected and offers a retry on a dead relay",
  () => {
    for (const status of ["offline", "error"] as const) {
      assertEquals(
        composerState({ kind: "relay", status, canWrite: false }),
        { notice: "not-connected", retry: true },
        status,
      );
    }
  },
);

Deno.test(
  "composerState says not connected without a retry where nothing reopens",
  () => {
    const dead: Array<Parameters<typeof composerState>[0]> = [
      { kind: "local", status: "offline", canWrite: false },
      { kind: "offline-view", status: "offline", canWrite: false },
    ];
    for (const c of dead) {
      assertEquals(
        composerState(c),
        { notice: "not-connected", retry: false },
        c.kind,
      );
    }
  },
);

// The note area's three states, as behavior: a string is said, an omitted
// prop takes the pairing sentence, and null renders nothing. The composer
// template dispatches on this, so each state owns its exit.
Deno.test("each note state renders its own way", () => {
  assertEquals(noticeRender("This conversation is not connected."), "say");
  assertEquals(noticeRender(undefined), "fallback");
  assertEquals(noticeRender(null), "none");
});
