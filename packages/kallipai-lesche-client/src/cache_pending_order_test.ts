// Pin the pending-flush ordering. localSeq is -Date.now(): the MOST
// negative value is the NEWEST row, so flushing and rehydrating must walk
// the rows in DESCENDING localSeq order (oldest first). An ascending sort
// silently flips that, and the reconnect pump -- a FIFO -- would then
// resend the newest line first: the server persists arrival order, so the
// inversion becomes permanent and publicly visible. Regression guard for
// exactly that flip.
//
// deno test has no IndexedDB, so the ordering is exercised through the
// exported pure sort that readPendingByTagma delegates to.

const { assertEquals } = await import("@std/assert");
const { sortPendingOldestFirst } = await import("./cache.ts");

Deno.test(
  "pending rows sort oldest-first under negative-millis localSeq",
  () => {
    const oldest = {
      tagmaId: "t-1",
      localSeq: -1000,
      text: "first try",
      createdAt: "2026-09-22T10:00:00Z",
    };
    const newest = {
      tagmaId: "t-1",
      localSeq: -2000,
      text: "second try",
      createdAt: "2026-09-22T10:00:01Z",
    };
    // IDB getAll returns ascending composite-key order = newest first.
    assertEquals(sortPendingOldestFirst([newest, oldest]), [oldest, newest]);
  },
);

Deno.test("an already-oldest-first sequence passes through untouched", () => {
  const oldest = { tagmaId: "t-1", localSeq: -1000, text: "a", createdAt: "x" };
  const middle = { tagmaId: "t-1", localSeq: -2000, text: "b", createdAt: "x" };
  const newest = { tagmaId: "t-1", localSeq: -3000, text: "c", createdAt: "x" };
  const rows = [oldest, middle, newest];
  assertEquals(sortPendingOldestFirst(rows), [oldest, middle, newest]);
});
