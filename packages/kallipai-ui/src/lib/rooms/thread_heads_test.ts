// Pins for the head a discussion is named by before its own page has read it:
// recorded once from the line the root sits on, cut to one short line, and
// bounded so a long session does not keep every discussion it ever opened.

import { assert, assertEquals } from "@std/assert";
import type { RoomLine } from "../room-message.ts";

// The module keeps its heads in `$state`, and this harness has no compiler:
// the rune is stubbed before the import, the way the trail's own chain test
// does it.
(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const {
  rememberThreadHead,
  rememberThreadHeadOf,
  threadHeadOf,
  THREAD_HEAD_LIMIT,
  THREAD_HEADS_MAX,
} = await import("./threadHeads.svelte.ts");

function line(text: string): RoomLine {
  return {
    seq: 1,
    senderId: "p-1",
    senderKind: "human",
    senderHandle: "@p-1",
    text,
    createdAt: "2026-11-15T00:00:00Z",
    mine: false,
  };
}

Deno.test("a head is recorded once and read back by room and root", () => {
  rememberThreadHead("room-a", 7, "the first words");
  assertEquals(threadHeadOf("room-a", 7), "the first words");
  rememberThreadHead("room-a", 7, "something else");
  assertEquals(
    threadHeadOf("room-a", 7),
    "the first words",
    "the root's own page is what names the discussion; a later write only repeats it",
  );
  assertEquals(threadHeadOf("room-a", 8), null);
  assertEquals(
    threadHeadOf("room-b", 7),
    null,
    "the same root in another room is another discussion",
  );
});

Deno.test("a head is one short line of the root's own text", () => {
  rememberThreadHeadOf("room-c", 3, line("alpha   beta\ngamma delta epsilon"));
  const head = threadHeadOf("room-c", 3);
  assert(head !== null);
  assertEquals(head, "alpha beta gamma del…");
  assert(
    [...head].length <= THREAD_HEAD_LIMIT + 1,
    "the trail carries a recognizable opening, not a second line of the bar",
  );
});

Deno.test("the oldest head leaves when the map is full", () => {
  for (let root = 0; root < THREAD_HEADS_MAX; root += 1) {
    rememberThreadHead("room-full", root, `head ${root}`);
  }
  assertEquals(threadHeadOf("room-full", 0), "head 0");
  rememberThreadHead("room-full", THREAD_HEADS_MAX, "the newest");
  assertEquals(threadHeadOf("room-full", 0), null, "the oldest left first");
  assertEquals(threadHeadOf("room-full", 1), "head 1");
  assertEquals(threadHeadOf("room-full", THREAD_HEADS_MAX), "the newest");
});
