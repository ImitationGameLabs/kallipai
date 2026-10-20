// Unit tests for the reveal controller: the row table, the scroll arguments,
// the focus move, the two spoken slots and the one brief mark both chat
// surfaces share. The module is rune-bearing and `deno test` runs it
// uncompiled, so it loads behind a passthrough $state shim plus an ambient
// declaration (the seam quotes_test uses).

import { assert, assertEquals, assertThrows } from "@std/assert";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { createReveal, revealMark } = await import("./reveal.svelte.ts");

type Options = {
  flashMs?: number;
  scope?: () => string | undefined;
  describe?: (id: number) => string | undefined;
};

/** A controller that names every row it reaches, the way a page's own
 *  `describe` closure does. */
function controller(options: Options = {}) {
  return createReveal({ describe: (id) => `row ${id}`, ...options });
}

/** A stand-in row element: counts the two things the controller does to it and
 *  keeps the arguments it was handed. */
function row() {
  const seen: {
    scrolls: number;
    focuses: number;
    scrollTo: ScrollIntoViewOptions | undefined;
    focusWith: FocusOptions | undefined;
  } = { scrolls: 0, focuses: 0, scrollTo: undefined, focusWith: undefined };
  const el = {
    scrollIntoView: (opts?: ScrollIntoViewOptions) => {
      seen.scrolls += 1;
      seen.scrollTo = opts;
    },
    focus: (opts?: FocusOptions) => {
      seen.focuses += 1;
      seen.focusWith = opts;
    },
  } as unknown as HTMLElement;
  return { el, seen };
}

Deno.test("a jump scrolls the row, focuses it, and says what it is", () => {
  const reveal = controller();
  const landed = row();
  const registered = reveal.trackRow(landed.el, 7);
  assertEquals(reveal.highlightId, null, "nothing marked before the jump");
  assertEquals(reveal.announcement, undefined, "and nothing said");
  assertEquals(reveal.jump(7), true);
  assertEquals(landed.seen.scrolls, 1, "the row is brought into view once");
  assertEquals(
    landed.seen.scrollTo,
    { block: "center", inline: "nearest" },
    "centred, so the reader sees the line and not just an edge",
  );
  assertEquals(landed.seen.focuses, 1, "the focus lands on it exactly once");
  assertEquals(
    landed.seen.focusWith,
    { preventScroll: true },
    "and does not drag the scroll back off the row",
  );
  assertEquals(reveal.announcement, "row 7", "and the jump says which row");
  assertEquals(reveal.highlightId, 7);
  registered.destroy();
});

Deno.test("an unnameable row is not jumped to at all", () => {
  const reveal = createReveal({ describe: () => undefined });
  const landed = row();
  reveal.trackRow(landed.el, 4);
  assertEquals(reveal.jump(4), false);
  assertEquals(landed.seen.scrolls, 0, "no row is scrolled for a silent jump");
  assertEquals(landed.seen.focuses, 0, "no focus is moved either");
  assertEquals(reveal.announcement, undefined);
  assertEquals(reveal.highlightId, null, "and nothing is marked");
});

Deno.test("the row is named before anything moves", () => {
  // The contract is an order, not a rescue: a page whose lookup throws has a
  // bug to fix, and the controller must not have moved the reader by then.
  const reveal = createReveal({
    describe: () => {
      throw new Error("no such row");
    },
  });
  const landed = row();
  reveal.trackRow(landed.el, 5);
  assertThrows(() => reveal.jump(5), Error, "no such row");
  assertEquals(landed.seen.scrolls, 0, "the row was not scrolled");
  assertEquals(landed.seen.focuses, 0, "and the focus did not move");
});

Deno.test("two rows called the same thing are still two announcements", () => {
  // A live region speaks on a change of content, and two rows can read alike
  // (same author, same minute). Each jump writes into the other slot, so the
  // second one is a change even though the words are the same.
  const reveal = createReveal({ describe: () => "the same sentence" });
  const first = row();
  const second = row();
  reveal.trackRow(first.el, 12);
  reveal.trackRow(second.el, 40);
  const slotBefore = reveal.slot;
  reveal.jump(12);
  const slotAfterFirst = reveal.slot;
  assert(slotAfterFirst !== slotBefore, "the first jump took a slot");
  reveal.jump(40);
  assert(reveal.slot !== slotAfterFirst, "the second jump took the other one");
  assertEquals(
    reveal.announcement,
    "the same sentence",
    "the words are the same, the announcement is not",
  );
});

Deno.test(
  "the mark and the sentence clear after the flash window",
  async () => {
    const reveal = controller({ flashMs: 5 });
    const landed = row();
    reveal.trackRow(landed.el, 3);
    reveal.jump(3);
    assertEquals(reveal.highlightId, 3);
    assertEquals(reveal.announcement, "row 3");
    await new Promise((resolve) => setTimeout(resolve, 25));
    assertEquals(reveal.highlightId, null);
    assertEquals(
      reveal.announcement,
      undefined,
      "a sentence has no life of its own after the moment it described",
    );
  },
);

Deno.test("an unregistered row cannot be jumped to", () => {
  const reveal = controller();
  assertEquals(reveal.jump(9), false);
  assertEquals(reveal.highlightId, null);
  assertEquals(reveal.announcement, undefined, "and nothing is said");
});

Deno.test("a jump onto a dropped row moves nothing", () => {
  const reveal = controller();
  const landed = row();
  const registered = reveal.trackRow(landed.el, 4);
  registered.destroy();
  assertEquals(reveal.jump(4), false);
  assertEquals(landed.seen.scrolls, 0);
  assertEquals(
    landed.seen.focuses,
    0,
    "no focus move without a row to land on",
  );
  assertEquals(reveal.announcement, undefined);
});

Deno.test("a refused jump leaves the focus and the sentence alone", () => {
  // A refused jump is not a jump: the row reached before keeps the focus, and
  // the sentence about it still stands.
  const reveal = controller({ flashMs: 10_000 });
  const landed = row();
  reveal.trackRow(landed.el, 1);
  reveal.jump(1);
  assertEquals(reveal.jump(99), false);
  assertEquals(
    landed.seen.focuses,
    1,
    "the focus stays on the row reached before",
  );
  assertEquals(reveal.announcement, "row 1", "and so does the sentence");
  reveal.destroy();
});

Deno.test("a later jump moves the focus and replaces the sentence", () => {
  const reveal = controller();
  const first = row();
  const second = row();
  reveal.trackRow(first.el, 3);
  reveal.trackRow(second.el, 4);
  reveal.jump(3);
  reveal.jump(4);
  assertEquals(first.seen.focuses, 1, "the first row keeps its landing");
  assertEquals(second.seen.focuses, 1, "the second row gains the focus");
  assertEquals(reveal.announcement, "row 4", "and the sentence follows it");
  reveal.destroy();
});

Deno.test("destroy drops the mark and the sentence", () => {
  const reveal = controller({ flashMs: 10_000 });
  const landed = row();
  reveal.trackRow(landed.el, 5);
  reveal.jump(5);
  assertEquals(reveal.highlightId, 5);
  assertEquals(reveal.announcement, "row 5");
  reveal.destroy();
  assertEquals(reveal.highlightId, null);
  assertEquals(reveal.announcement, undefined);
});

Deno.test("the later jump owns the mark", async () => {
  // Two jumps in quick succession: the later one must win, and the earlier
  // timer must not clear it early.
  const reveal = controller({ flashMs: 250 });
  const landed = row();
  reveal.trackRow(landed.el, 3);
  reveal.trackRow(landed.el, 4);
  reveal.jump(3);
  await new Promise((resolve) => setTimeout(resolve, 180));
  reveal.jump(4);
  assertEquals(reveal.highlightId, 4);
  // Past the first reveal's window: the stale timer is gone, so the mark holds.
  await new Promise((resolve) => setTimeout(resolve, 140));
  assertEquals(reveal.highlightId, 4);
  await new Promise((resolve) => setTimeout(resolve, 180));
  assertEquals(reveal.highlightId, null);
  assertEquals(landed.seen.scrolls, 2);
});

Deno.test("only the revealed row wears the mark", () => {
  const mark = "rounded-base ring-2 ring-primary-500";
  assertEquals(revealMark(7, 7), mark);
  assertEquals(revealMark(7, 8), "", "a neighbour stays plain");
  assertEquals(revealMark(null, 7), "", "nothing revealed, nothing marked");
});

Deno.test("a scope swap drops the mark and the sentence it left behind", () => {
  // Both are about a row number, and row numbers restart in the next session:
  // a flash or a sentence outliving the swap would describe a stranger row.
  let scope = "room-a";
  const reveal = controller({ flashMs: 10_000, scope: () => scope });
  const landed = row();
  reveal.trackRow(landed.el, 12);
  reveal.jump(12);
  assertEquals(reveal.highlightId, 12);
  assertEquals(reveal.announcement, "row 12");
  scope = "room-b";
  assertEquals(reveal.highlightId, null);
  assertEquals(
    reveal.announcement,
    undefined,
    "about the row it no longer shows",
  );
});

Deno.test("a mark inside its own scope survives reads", () => {
  let scope = "room-a";
  const reveal = controller({ flashMs: 10_000, scope: () => scope });
  reveal.trackRow(row().el, 4);
  reveal.jump(4);
  assertEquals(reveal.highlightId, 4, "same scope: the mark holds");
  assertEquals(reveal.announcement, "row 4", "and so does the sentence");
  scope = "room-b";
  scope = "room-a";
  assertEquals(reveal.highlightId, 4, "returning is not a swap away");
  assertEquals(reveal.announcement, "row 4");
});

Deno.test("one jump serves both row tables", () => {
  // The bilateral transcript keys rows by history id and the room transcript by
  // seq. The controller hands the id to the closure and asks nothing more, so
  // each surface's sentence comes from its own table.
  const byHistoryId = new Map([[7, "history row 7"]]);
  const bySeq = new Map([[7, "room row 7"]]);
  const bilateral = createReveal({ describe: (id) => byHistoryId.get(id) });
  const room = createReveal({ describe: (id) => bySeq.get(id) });
  const historyRow = row();
  const seqRow = row();
  bilateral.trackRow(historyRow.el, 7);
  room.trackRow(seqRow.el, 7);
  assertEquals(bilateral.jump(7), true);
  assertEquals(room.jump(7), true);
  assertEquals(bilateral.announcement, "history row 7");
  assertEquals(room.announcement, "room row 7", "each reads its own table");
});
