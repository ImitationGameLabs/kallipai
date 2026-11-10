// The two cache hydrates land different things, and both land the rows.
// `restore` shows a window with no stream to resume from; `restoreResume`
// shows a live window and seeds the positions the reader continues at. A
// hydrate that seeds a cursor and drops the rows is silent -- the transcript
// renders empty while every cursor assertion still passes -- so each entry
// asserts all three: rows, row-id edges, cursors.
//
// The module under test is rune-bearing, so a passthrough `$state` shim lets
// `deno test` load it uncompiled (the same seam cache_quote_test uses).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { Transcript } = await import("./transcript.svelte.ts");

type Line = {
  historyId: number;
  role: "assistant";
  text: string;
  seq?: number;
};

function line(historyId: number, seq?: number): Line {
  return { historyId, role: "assistant", text: `row ${historyId}`, seq };
}

function transcript() {
  return new Transcript("conv-1", () => {});
}

/** Three cached rows whose positions run 1..3 under ids 7..9. */
const WINDOW = [line(7, 1), line(8, 2), line(9, 3)];

Deno.test(
  "restore lands the rows and both row-id edges, and no position",
  () => {
    const t = transcript();
    t.restore(WINDOW);
    assertEquals(
      t.rows.map((l) => l.historyId),
      [7, 8, 9],
      "the rows land",
    );
    assertEquals(
      [t.minRowId, t.maxRowId],
      [7, 9],
      "both edges follow the rows",
    );
    assertEquals(
      [t.lastSeq, t.minSeq],
      [0, 0],
      "an offline window has no position",
    );
  },
);

Deno.test(
  "restoreResume lands the rows, both edges, and the resume cursor",
  () => {
    const t = transcript();
    t.restoreResume(WINDOW);
    assertEquals(
      t.rows.map((l) => l.historyId),
      [7, 8, 9],
      "the rows land",
    );
    assertEquals(
      [t.minRowId, t.maxRowId],
      [7, 9],
      "both edges follow the rows",
    );
    assertEquals(t.lastSeq, 3, "the newest rendered position seeds the cursor");
    assertEquals(t.minSeq, 1, "the oldest picks up the scroll-up anchor");
  },
);

Deno.test(
  "a frozen window still shows its cached rows and places nothing",
  () => {
    const t = transcript();
    t.historyError = "protocol";
    t.restoreResume([line(7), line(8), line(9)]);
    assertEquals(t.rows.length, 3, "content lands under a freeze");
    assertEquals(t.lastSeq, 0, "no position is adopted");
  },
);
