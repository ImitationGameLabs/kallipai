// Tests for the headless auto-scroll controller's miss counting. The counting
// fence is extracted as the pure missedLines() so the prepend-vs-append
// distinction is testable without a viewport (the badgeLabel precedent in
// session/unread.svelte.ts); the controller's own runes wiring has no
// runtime test harness here (the composer_test rationale), so it gets
// source-read pins instead.

import { assert, assertEquals } from "@std/assert";
import { missedLines, type TailSnapshot } from "./transcript.svelte.ts";

const MODULE = new URL("./transcript.svelte.ts", import.meta.url);
const CV = new URL("../components/ConversationView.svelte", import.meta.url);
const ROOM = new URL("../pages/RoomConversationPage.svelte", import.meta.url);
const CHANNEL = new URL("../pages/ChannelChatPage.svelte", import.meta.url);
const DIRECT = new URL("../pages/DirectSessionPage.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

const tail = (length: number, key?: string | number): TailSnapshot => ({
  length,
  key,
});

Deno.test("missedLines counts appended lines while detached", () => {
  assertEquals(missedLines(tail(5, 10), tail(8, 13), false), 3);
});

Deno.test("missedLines ignores a grown array whose tail did not move", () => {
  // A history prepend pages older lines in above the tail: the array grows
  // but the last line is unchanged, so nothing counts as new.
  assertEquals(missedLines(tail(5, 10), tail(105, 10), false), 0);
});

Deno.test("missedLines counts nothing while following", () => {
  assertEquals(missedLines(tail(5, 10), tail(9, 14), true), 0);
});

Deno.test(
  "missedLines counts nothing when content shrinks or stands still",
  () => {
    assertEquals(missedLines(tail(8, 13), tail(5, 10), false), 0);
    assertEquals(missedLines(tail(8, 13), tail(8, 13), false), 0);
  },
);

Deno.test(
  "the controller wires the fence, the reset, and the reactive count",
  { permissions: { read: [MODULE] } },
  () => {
    const src = source(MODULE);
    // stick() counts only through the missedLines fence (the prepend
    // guard), and the count drops when follow returns by hand or by
    // forceBottom.
    assert(src.includes("missed += missedLines("));
    assert(src.includes("if (follow) missed = 0;"));
    const fb = src.indexOf("function forceBottom");
    assert(fb >= 0, "forceBottom must exist");
    const block = src.slice(fb, src.indexOf("}", fb));
    assert(block.includes("follow = true;"));
    assert(block.includes("missed = 0;"));
    // The count is exposed reactively and the action is returned.
    assert(src.includes("get missed()"));
    assert(src.includes("forceBottom,"));
  },
);

Deno.test(
  "reset restores fresh-mount state and keeps the viewport binding",
  { permissions: { read: [MODULE] } },
  () => {
    const src = source(MODULE);
    const rb = src.indexOf("function reset");
    assert(rb >= 0, "reset must exist");
    const block = src.slice(rb, src.indexOf("return {", rb));
    assert(block.includes("follow = true;"));
    assert(block.includes("missed = 0;"));
    assert(block.includes("lastTail = { length: 0, key: undefined }"));
    // The viewport binding survives a reset: no re-bind, no scroll write.
    assert(!block.includes("viewport"));
  },
);

Deno.test(
  "both mount points reset on session identity changes",
  { permissions: { read: [MODULE, CV, ROOM, CHANNEL, DIRECT] } },
  () => {
    // The shared render body resets when the feeding session changes...
    const cv = source(CV);
    assert(cv.includes("void sessionKey;"));
    assert(cv.includes("scroll.reset()"));
    // ...the channel-like callers hand it the route identity...
    assert(source(CHANNEL).includes("sessionKey={conversationId}"));
    assert(source(DIRECT).includes("${tagmaId}:${peerId}"));
    // ...and the room page keys on its own room param.
    const room = source(ROOM);
    assert(room.includes("void roomId;"));
    assert(room.includes("scroll.reset()"));
  },
);

Deno.test(
  "the session-key reset is declared before the stick effect",
  { permissions: { read: [CV, ROOM] } },
  () => {
    for (const src of [source(CV), source(ROOM)]) {
      const resetAt = src.indexOf("scroll.reset()");
      const stickAt = src.indexOf("scroll.stick(");
      assert(resetAt >= 0 && stickAt >= 0);
      // Effects in the same flush run in declaration order: the reset
      // must be declared first or stick() observes a stale session.
      assert(
        resetAt < stickAt,
        "reset effect must be declared before the stick effect",
      );
    }
  },
);
