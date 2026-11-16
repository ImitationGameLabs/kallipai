// Tests for the unread store badge model: BOTH kinds of badge are assigned by
// a server -- a room's count is the lesche's answer for this member (the badge
// says what reached them, which a client cannot recompute from the rows it
// pulled), a 1:1's is the tagma's stamped count. The store never derives a
// number locally, orders 1:1 stamps by their cursor, suppresses room counts and
// 1:1 badges while the page is open, and coalesces room cursor writes into one
// request per throttle window.
// Rune-bearing module under `deno test`: passthrough $state shim (the
// statusCard_test pattern). All IO seams are stubbed; no network, no real
// timers.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { unreadStore, badgeLabel, roomKey, tagmaKey } =
  await import("./unread.svelte.ts");

// -- seams -------------------------------------------------------------------

let armed: (() => void)[] = [];
let puts: [string, number][] = [];
let fetches: string[] = [];
let counts: Map<string, { unread: number; cursor: number }>;
let failNext = false;
let putFails = false;
// When gated, a count read hangs until the test releases it -- the way a real
// request sits in flight while more envelopes land.
let gated = false;
let releases: (() => void)[] = [];

function installSeams(): void {
  unreadStore.setTimerArmer((fn) => {
    armed.push(fn);
    return 0 as unknown as ReturnType<typeof setTimeout>;
  });
  unreadStore.setReadCursorPutter((roomId, seq) => {
    puts.push([roomId, seq]);
    if (putFails) {
      putFails = false;
      return Promise.reject(new Error("cursor write failed"));
    }
    return Promise.resolve();
  });
  unreadStore.setRoomUnreadFetcher((roomId) => {
    fetches.push(roomId);
    if (failNext) {
      failNext = false;
      return Promise.reject(new Error("count read failed"));
    }
    const answer = () => counts.get(roomId) ?? { unread: 0, cursor: 0 };
    if (!gated) return Promise.resolve(answer());
    return new Promise((resolve) => {
      releases.push(() => resolve(answer()));
    });
  });
}

/** Drain pending microtasks so fire-and-forget reads settle. */
const flush = () => new Promise<void>((r) => setTimeout(r, 0));

function baseline(): void {
  armed = [];
  puts = [];
  fetches = [];
  counts = new Map();
  failNext = false;
  putFails = false;
  gated = false;
  releases = [];
  installSeams();
  unreadStore.reset();
}

// -- 1:1 badges: assigned, never accumulated ---------------------------------

Deno.test("a second 1:1 stamp overwrites the first instead of adding", () => {
  baseline();
  unreadStore.setTagmaUnread("t1", { unread: 4, cursor: 9 });
  assertEquals(unreadStore.countOf(tagmaKey("t1")), 4);
  // A later stamp from a higher cursor is a new fact, not a delta: a device
  // syncing a long backlog must show its number once, not tick per row.
  unreadStore.setTagmaUnread("t1", { unread: 7, cursor: 12 });
  assertEquals(unreadStore.countOf(tagmaKey("t1")), 7);
  // The tagma's answer to a cursor write is a smaller number again.
  unreadStore.setTagmaUnread("t1", { unread: 0, cursor: 20 });
  assertEquals(unreadStore.countOf(tagmaKey("t1")), 0);
});

Deno.test("a stale or duplicate stamp never regresses the badge", () => {
  baseline();
  unreadStore.setTagmaUnread("t1", { unread: 6, cursor: 12 });
  // An older snapshot (one carrier overtook another en route) is dropped.
  unreadStore.setTagmaUnread("t1", { unread: 9, cursor: 11 });
  assertEquals(unreadStore.countOf(tagmaKey("t1")), 6);
  // The same cursor with a smaller count is a duplicate.
  unreadStore.setTagmaUnread("t1", { unread: 5, cursor: 12 });
  assertEquals(unreadStore.countOf(tagmaKey("t1")), 6);
});

Deno.test("a stamp that lands while viewing holds the badge at zero", () => {
  baseline();
  unreadStore.enter(tagmaKey("t1"));
  unreadStore.setTagmaUnread("t1", { unread: 5, cursor: 7 });
  assertEquals(unreadStore.countOf(tagmaKey("t1")), 0);
  // Leaving ends the viewing flag; the tagma's answer to the open (cursor 9,
  // count 0) is what shows, and an unrelated key is untouched.
  unreadStore.leaveTagma("t1");
  unreadStore.setTagmaUnread("t1", { unread: 0, cursor: 9 });
  assertEquals(unreadStore.countOf(tagmaKey("t1")), 0);
  unreadStore.setTagmaUnread("t2", { unread: 3, cursor: 1 });
  assertEquals(unreadStore.countOf(tagmaKey("t2")), 3);
});

Deno.test("a stamp masked by viewing returns when the page unmounts", () => {
  baseline();
  unreadStore.setTagmaUnread("t1", { unread: 5, cursor: 7 });
  assertEquals(unreadStore.countOf(tagmaKey("t1")), 5);
  // Mounting the page hides the badge but keeps the tagma's number: a stamp
  // computed before the open's cursor write landed must not lower the bar the
  // next stamp has to clear.
  unreadStore.enter(tagmaKey("t1"));
  assertEquals(unreadStore.countOf(tagmaKey("t1")), 0);
  unreadStore.setTagmaUnread("t1", { unread: 9, cursor: 7 });
  assertEquals(unreadStore.countOf(tagmaKey("t1")), 0); // still masked
  // Unmounting shows the stamped count again, not the zero the mask left.
  unreadStore.leaveTagma("t1");
  assertEquals(unreadStore.countOf(tagmaKey("t1")), 9);
});

Deno.test(
  "explicit open clears only the viewed key; other keys keep their count",
  () => {
    baseline();
    unreadStore.setTagmaUnread("t1", { unread: 2, cursor: 4 });
    unreadStore.setTagmaUnread("t2", { unread: 1, cursor: 4 });
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 2);
    assertEquals(unreadStore.countOf(tagmaKey("t2")), 1);
    // The ONLY clear path is enter() from the conversation page's mount effect
    // (boot auto-open never calls the store -- see channels.ensureOpen: it has
    // no unread surface). A registry re-list must not clear either.
    unreadStore.initRooms([]);
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 2);
    unreadStore.enter(tagmaKey("t1"));
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 0);
    assertEquals(unreadStore.countOf(tagmaKey("t2")), 1);
  },
);

Deno.test("the bar total is the sum across conversations", async () => {
  baseline();
  unreadStore.setTagmaUnread("t1", { unread: 2, cursor: 3 });
  counts.set("r1", { unread: 3, cursor: 0 });
  unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
  await flush();
  assertEquals(unreadStore.total(), 5);
});

// -- rooms: the count is the server's answer --------------------------------

Deno.test("a room badge is assigned from the server count", async () => {
  baseline();
  counts.set("r1", { unread: 3, cursor: 0 });
  unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
  await flush();
  assertEquals(unreadStore.countOf(roomKey("r1")), 3);
  // A later envelope replaces the number with the newer answer; it never adds
  // to it, so a burst cannot double-count.
  counts.set("r1", { unread: 4, cursor: 0 });
  unreadStore.noteRoomActivity("r1");
  await flush();
  assertEquals(unreadStore.countOf(roomKey("r1")), 4);
  assertEquals(fetches, ["r1", "r1"]);
});

Deno.test("a count above the cap still renders as 99+", async () => {
  baseline();
  counts.set("r1", { unread: 250, cursor: 0 });
  unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
  await flush();
  assertEquals(unreadStore.countOf(roomKey("r1")), 250);
  assertEquals(badgeLabel(unreadStore.countOf(roomKey("r1"))), "99+");
});

Deno.test("badgeLabel caps the display at the cap", () => {
  assertEquals(badgeLabel(0), "0");
  assertEquals(badgeLabel(5), "5");
  assertEquals(badgeLabel(99), "99");
  assertEquals(badgeLabel(100), "99+");
});

Deno.test("a cursor echo asks again; the store does not subtract", async () => {
  baseline();
  counts.set("r1", { unread: 5, cursor: 10 });
  unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 10 }]);
  await flush();
  assertEquals(unreadStore.countOf(roomKey("r1")), 5);
  // Another session read through 13: the answer is the server's, and the
  // store never derives it from the watermark advance.
  counts.set("r1", { unread: 2, cursor: 13 });
  unreadStore.applyServerRead("r1", 13);
  await flush();
  assertEquals(unreadStore.countOf(roomKey("r1")), 2);
  // A stale echo below the stored watermark asks nothing.
  const before = fetches.length;
  unreadStore.applyServerRead("r1", 12);
  await flush();
  assertEquals(fetches.length, before);
  assertEquals(unreadStore.countOf(roomKey("r1")), 2);
  // A jump past everything zeroes because the server says so.
  counts.set("r1", { unread: 0, cursor: 40 });
  unreadStore.applyServerRead("r1", 40);
  await flush();
  assertEquals(unreadStore.countOf(roomKey("r1")), 0);
});

Deno.test(
  "tab scope: a fresh store converges from server truth (no cross-tab broadcast)",
  async () => {
    baseline();
    counts.set("r1", { unread: 5, cursor: 0 });
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 5);
    // A second tab starts from the registry watermark (its own store state)
    // and converges via the list + its own read -- there is no cross-tab
    // broadcast, and the count comes from server truth, not inherited.
    counts.set("r1", { unread: 0, cursor: 15 });
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 15 }]);
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 0);
  },
);

Deno.test(
  "a burst during a read lands on the last answer, one read in flight",
  async () => {
    baseline();
    gated = true;
    counts.set("r1", { unread: 1, cursor: 0 });
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
    assertEquals(fetches, ["r1"]);
    // Two more envelopes arrive while that read is still open: they queue one
    // trailing pass instead of stacking requests.
    unreadStore.noteRoomActivity("r1");
    unreadStore.noteRoomActivity("r1");
    assertEquals(fetches, ["r1"]);
    counts.set("r1", { unread: 5, cursor: 0 });
    releases.shift()!();
    await flush();
    assertEquals(fetches, ["r1", "r1"]);
    releases.shift()!();
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 5);
  },
);

Deno.test(
  "a failed read leaves the badge alone and the next event asks again",
  async () => {
    baseline();
    counts.set("r1", { unread: 4, cursor: 0 });
    failNext = true;
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 0);
    unreadStore.noteRoomActivity("r1");
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 4);
  },
);

Deno.test(
  "viewing: count holds at zero, no read while open, one after the flush",
  async () => {
    baseline();
    counts.set("r1", { unread: 3, cursor: 0 });
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 3);

    unreadStore.enter(roomKey("r1"));
    assertEquals(unreadStore.countOf(roomKey("r1")), 0);
    assertEquals(armed.length, 1); // the enter-scheduled write window
    assertEquals(puts.length, 0);

    // Envelopes while viewing ask for nothing: the answer would be masked.
    unreadStore.noteRoomActivity("r1");
    await flush();
    assertEquals(fetches, ["r1"]);

    // Rendered lines advance the cursor, coalesced into the open window.
    unreadStore.noteViewedLines("r1", 1);
    unreadStore.noteViewedLines("r1", 2);
    unreadStore.noteViewedLines("r1", 3);
    assertEquals(armed.length, 1); // still one window, no per-line requests
    assertEquals(puts.length, 0);
    armed[0]!(); // the window elapses -> one merged write with the latest cursor
    await flush();
    assertEquals(puts, [["r1", 3]]);

    // Leaving flushes the final cursor, then asks for the count behind it.
    counts.set("r1", { unread: 0, cursor: 9 });
    unreadStore.leaveRoom("r1", 9);
    await flush();
    assertEquals(puts, [
      ["r1", 3],
      ["r1", 9],
    ]);
    assertEquals(fetches, ["r1", "r1"]);
    assertEquals(unreadStore.countOf(roomKey("r1")), 0);
  },
);

Deno.test("a room dropped from the list drops its badge", async () => {
  baseline();
  counts.set("r1", { unread: 6, cursor: 0 });
  unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
  await flush();
  assertEquals(unreadStore.countOf(roomKey("r1")), 6);
  unreadStore.initRooms([]);
  assertEquals(unreadStore.countOf(roomKey("r1")), 0);
  assertEquals(unreadStore.total(), 0);
});

Deno.test(
  "a failed cursor write on leave claims nothing the server never recorded",
  async () => {
    baseline();
    counts.set("r1", { unread: 2, cursor: 0 });
    unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 2);

    unreadStore.enter(roomKey("r1"));
    unreadStore.noteViewedLines("r1", 5);
    // The write fails, so the watermark must not move: the answer behind it
    // still speaks about cursor 0 and is the truth about what the server holds.
    // Its number is deliberately not the one already shown: an implementation
    // that raised the read watermark before the write would drop this answer.
    putFails = true;
    counts.set("r1", { unread: 7, cursor: 0 });
    unreadStore.leaveRoom("r1", 5);
    await flush();
    assertEquals(unreadStore.countOf(roomKey("r1")), 7);
  },
);

Deno.test("a count answer older than a landed cursor is dropped", async () => {
  baseline();
  counts.set("r1", { unread: 4, cursor: 0 });
  unreadStore.initRooms([{ roomId: "r1", lastReadSeq: 0 }]);
  await flush();
  assertEquals(unreadStore.countOf(roomKey("r1")), 4);

  // A newer answer lands: nothing past cursor 6.
  counts.set("r1", { unread: 0, cursor: 6 });
  unreadStore.noteRoomActivity("r1");
  await flush();
  assertEquals(unreadStore.countOf(roomKey("r1")), 0);

  // A stale answer (taken before that cursor) arrives next; showing it would
  // relight rows the member has read.
  counts.set("r1", { unread: 4, cursor: 0 });
  unreadStore.noteRoomActivity("r1");
  await flush();
  assertEquals(unreadStore.countOf(roomKey("r1")), 0);
});

Deno.test(
  "the stamped count survives viewing so a replay cannot beat it",
  () => {
    baseline();
    unreadStore.setTagmaUnread("t1", { unread: 5, cursor: 5 });
    assertEquals(unreadStore.countOrUndefined(tagmaKey("t1")), 5);

    // Opening the page zeroes the badge but not the fact a later frame has to
    // beat: the replay every new stream sends (5 at cursor 5) must not read as
    // an arrival, or the row jumps to the top claiming "just now".
    unreadStore.enter(tagmaKey("t1"));
    assertEquals(unreadStore.countOf(tagmaKey("t1")), 0);
    assertEquals(unreadStore.countOrUndefined(tagmaKey("t1")), 5);
  },
);

Deno.test("a page opened before any count leaves the bar unset", () => {
  baseline();
  // Opening the conversation first seats an entry with a zero badge. That zero
  // is a seed, not a fact: a replayed 5 after a reconnect must not beat it and
  // stamp days-old backlog as an arrival, so the bar stays undefined.
  unreadStore.enter(tagmaKey("t1"));
  assertEquals(unreadStore.countOrUndefined(tagmaKey("t1")), undefined);

  unreadStore.setTagmaUnread("t1", { unread: 5, cursor: 5 });
  assertEquals(unreadStore.countOrUndefined(tagmaKey("t1")), 5);
});
