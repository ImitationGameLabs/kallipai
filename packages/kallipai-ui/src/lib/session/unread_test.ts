// Tests for the unread store badge model: the per-key watermark
// fence counts every source exactly once (1:1 line-entry hook, room
// catch-up pull), explicit-open viewing clears without touching other keys,
// the cursor-changed reduction converges order-safely, and the room cursor
// write coalesces into one request per throttle window. Rune-bearing module
// under `deno test`: passthrough $state shim (the statusCard_test pattern).
// The store's IndexedDB watermark persistence degrades to null here (no IDB
// in deno), which is exactly the seed-pending branch a fresh device takes;
// the durable round-trip is a browser walkthrough concern, not a unit
// one. All IO seams are stubbed; no network, no real timers.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { unreadStore, badgeLabel, roomKey, tagmaKey, CATCHUP_PAGE } =
  await import("./unread.svelte.ts");

// -- seams -------------------------------------------------------------------

let armed: (() => void)[] = [];
let puts: [string, number][] = [];
let selfId: string | null = "me";
let pages: Map<string, { seq: number; senderId: string }[]>;

function installSeams(): void {
  unreadStore.setTimerArmer((fn) => {
    armed.push(fn);
    return 0 as unknown as ReturnType<typeof setTimeout>;
  });
  unreadStore.setReadCursorPutter((roomId, seq) => {
    puts.push([roomId, seq]);
    return Promise.resolve();
  });
  unreadStore.setSelfId(() => selfId);
  unreadStore.setRoomPageFetcher((roomId, afterSeq, limit) => {
    assertEquals(limit, CATCHUP_PAGE);
    return Promise.resolve(
      (pages.get(roomId) ?? []).filter((r) => r.seq > afterSeq).slice(0, limit),
    );
  });
}

/** Drain pending microtasks so fire-and-forget pulls settle. */
const flush = () => new Promise<void>((r) => setTimeout(r, 0));

function baseline(): void {
  armed = [];
  puts = [];
  selfId = "me";
  pages = new Map();
  installSeams();
  unreadStore.reset();
}

Deno.test(
  "1:1 line entry: new ids count, replays at/below the fence do not",
  async () => {
    baseline();
    unreadStore.hydrateTagma("t1", 3);
    unreadStore.observeTagmaLine("t1", 5);
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 1);
    unreadStore.observeTagmaLine("t1", 5); // replay (== knownSeq)
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 1);
    unreadStore.observeTagmaLine("t1", 4); // replay (< knownSeq, late catch-up row)
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 1);
    unreadStore.observeTagmaLine("t1", 6);
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 2);
    await flush();
  },
);

Deno.test(
  "explicit open clears only the viewed key; other keys keep their count",
  () => {
    baseline();
    unreadStore.hydrateTagma("t1", 3);
    unreadStore.hydrateTagma("t2", 3);
    unreadStore.observeTagmaLine("t1", 4);
    unreadStore.observeTagmaLine("t1", 5);
    unreadStore.observeTagmaLine("t2", 4);
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 2);
    assertEquals(unreadStore.countOf(tagmaKey("t2")), 1);
    // The ONLY clear path is enter() from the conversation page's mount effect
    // (boot auto-open never calls the store -- see channels.ensureOpen: it has
    // no unread surface). A registry re-list must not clear either.
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 2);
    unreadStore.enter(tagmaKey("t1"));
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 0);
    assertEquals(unreadStore.countOf(tagmaKey("t2")), 1);
  },
);

Deno.test(
  "room pull counts precisely, skips own lines; a full page reads 99+",
  async () => {
    baseline();
    pages.set("r1", [
      { seq: 1, senderId: "peer" },
      { seq: 2, senderId: "peer" },
      { seq: 3, senderId: "me" },
      { seq: 4, senderId: "peer" },
    ]);
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
    await flush();
    // 4 rows, 1 mine -> 3; my own row still advances the fence.
    assertEquals(unreadStore.countOf(roomKey("r1")), 3);
    unreadStore.noteRoomActivity("r1");
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 3); // no double-count

    const fullPage = Array.from({ length: CATCHUP_PAGE }, (_, i) => ({
      seq: i + 1,
      senderId: "peer",
    }));
    pages.set("r2", fullPage);
    unreadStore.initRooms([{ roomId: "r2", lastReadSeq: 0 }]);
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r2")), CATCHUP_PAGE);
    assertEquals(badgeLabel(unreadStore.countOf(roomKey("r2"))), "99+");
  },
);

Deno.test("badgeLabel caps the display at the cap", () => {
  assertEquals(badgeLabel(0), "0");
  assertEquals(badgeLabel(5), "5");
  assertEquals(badgeLabel(99), "99");
  assertEquals(badgeLabel(100), "99+");
});

Deno.test("the bar total is the sum across conversations", async () => {
  baseline();
  unreadStore.hydrateTagma("t1", 0);
  unreadStore.observeTagmaLine("t1", 1);
  unreadStore.observeTagmaLine("t1", 2);
  pages.set("r1", [
    { seq: 1, senderId: "p" },
    { seq: 2, senderId: "p" },
    { seq: 3, senderId: "p" },
  ]);
  unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
  await flush();
  assertEquals(unreadStore.total(), 5);
});

Deno.test(
  "cursor-changed reduces by the advance; stale events never regress",
  async () => {
    baseline();
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 10 }]);
    pages.set(
      "r1",
      [11, 12, 13, 14, 15].map((seq) => ({ seq, senderId: "peer" })),
    );
    unreadStore.noteRoomActivity("r1");
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 5);
    // Another session read through 13: exactly 11..13 leave the count.
    unreadStore.applyServerRead("r1", 13);
    assertEquals(unreadStore.countOf(roomKey("r1")), 2);
    // A stale/duplicated echo below the newest watermark is a no-op.
    unreadStore.applyServerRead("r1", 12);
    assertEquals(unreadStore.countOf(roomKey("r1")), 2);
    // A jump past everything we have seen zeroes without going negative.
    unreadStore.applyServerRead("r1", 40);
    assertEquals(unreadStore.countOf(roomKey("r1")), 0);
  },
);

Deno.test(
  "watermark rehydration: restored fence counts the delta, null seeds",
  async () => {
    baseline();
    // A restored watermark: the reload's catch-up replays nothing below it.
    unreadStore.hydrateTagma("t1", 240);
    unreadStore.observeTagmaLine("t1", 240);
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 0);
    unreadStore.observeTagmaLine("t1", 241);
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 1);
    // No stored watermark (fresh device): the FIRST observed line seeds the
    // fence instead of counting the whole replay as unread.
    unreadStore.hydrateTagma("t2", null);
    unreadStore.observeTagmaLine("t2", 100);
    assertEquals(unreadStore.countOf(tagmaKey("t2")), 0);
    unreadStore.observeTagmaLine("t2", 101);
    assertEquals(unreadStore.countOf(tagmaKey("t2")), 1);
    await flush();
  },
);

Deno.test(
  "tab scope: a fresh store converges from server truth (no cross-tab broadcast)",
  async () => {
    baseline();
    pages.set(
      "r1",
      [11, 12, 13, 14, 15].map((seq) => ({ seq, senderId: "peer" })),
    );
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 5);
    // A second tab starts from the registry watermark (its own store state)
    // and converges via the list + its own pull -- there is no cross-tab
    // broadcast, and the count is recomputed from server truth, not inherited.
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 15 }]);
    assertEquals(unreadStore.countOf(roomKey("r1")), 0);
    unreadStore.noteRoomActivity("r1");
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 0);
  },
);

Deno.test(
  "viewing: count holds at zero, cursor writes coalesce, leave flushes",
  async () => {
    baseline();
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
    await flush();
    unreadStore.enter(roomKey("r1"));
    assertEquals(armed.length, 1); // the enter-scheduled write window
    assertEquals(puts.length, 0);
    // New lines arrive while viewing: the page's line tick advances the fence
    // (count stays 0) and the writes coalesce into the open window.
    pages.set(
      "r1",
      [1, 2, 3].map((seq) => ({ seq, senderId: "peer" })),
    );
    unreadStore.noteViewedLines("r1", 1);
    unreadStore.noteViewedLines("r1", 2);
    unreadStore.noteViewedLines("r1", 3);
    assertEquals(armed.length, 1); // still one window, no per-line requests
    assertEquals(puts.length, 0);
    armed[0](); // the window elapses -> one merged write with the latest cursor
    await flush();
    assertEquals(puts, [["r1", 3]]);
    // Switching away flushes the final cursor at once.
    unreadStore.leaveRoom("r1", 9);
    await flush();
    assertEquals(puts, [
      ["r1", 3],
      ["r1", 9],
    ]);
  },
);
