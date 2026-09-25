// Tests for the direct-session store's merge legs (canonical-min dedup across
// the two daemons' views of one pair, max-side-only retention, min-side
// replacement, prune aging) and the transcript fetch's bounded 502 downgrade
// (the manage bridge drops an oversized page WHOLE, so the same
// params must never be retried -- the limit halves to a floor of 1).
//
// Seams (the channels_refresh_test pattern): a Harness subclass scripts
// fetchDirectRows / manage / rootId -- the store's contract with the bridge
// plumbing is exactly those three legs -- and drives ticks directly. The
// module under test transitively imports rune-bearing stores, so the $state /
// $derived passthrough shims are declared before the dynamic imports (deno
// test runs the modules uncompiled).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
  function $derived<T>(expr: T): T;
}
(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;
(globalThis as Record<string, unknown>)["$derived"] = (v: unknown) => v;

const { assertEquals, assertRejects } = await import("@std/assert");
const { DirectSessionsStore } = await import("./directSessions.svelte.ts");
type DirectSessionEntry =
  import("./directSessions.svelte.ts").DirectSessionEntry;

const A = "aaaaaaaa-0000-0000-0000-000000000001";
const B = "bbbbbbbb-0000-0000-0000-000000000002";

function entry(
  tagmaId: string,
  peerId: string,
  peerHandle = "",
): DirectSessionEntry {
  return { tagmaId, peerId, peerHandle, sessionId: `s-${tagmaId}` };
}

class Harness extends DirectSessionsStore {
  /** tagmaId → scripted rows (or a thrown error for a dead daemon). */
  script = new Map<string, DirectSessionEntry[] | Error>();
  /** Paths seen by the fake manage bridge, in order. */
  calls: string[] = [];
  /** manage responses, popped per call. */
  manageScript: Array<{ status: number; body?: unknown }> = [];

  override fetchDirectRows(tagmaId: string): Promise<DirectSessionEntry[]> {
    const next = this.script.get(tagmaId);
    if (next instanceof Error) throw next;
    return Promise.resolve(next ?? []);
  }

  override manage(
    _tagmaId: string,
    _method: string,
    path: string,
  ): Promise<{ status: number; body: unknown }> {
    this.calls.push(path);
    const step = this.manageScript.shift() ?? { status: 200, body: [] };
    return Promise.resolve({ status: step.status, body: step.body ?? [] });
  }

  override rootId(_tagmaId: string): Promise<string> {
    return Promise.resolve("root-1");
  }
}

Deno.test("both daemons reporting a pair keep the min-side row", async () => {
  const h = new Harness();
  h.script.set(A, [entry(A, B, "b@owner")]);
  h.script.set(B, [entry(B, A)]);
  await h.refreshTagma(A, 1);
  await h.refreshTagma(B, 1);
  const rows = h.list();
  assertEquals(rows.length, 1);
  assertEquals(rows[0].tagmaId, A);
  assertEquals(rows[0].peerHandle, "b@owner");
});

Deno.test("a pair only the max side reports is kept", async () => {
  const h = new Harness();
  h.script.set(B, [entry(B, A)]);
  await h.refreshTagma(B, 1);
  const rows = h.list();
  assertEquals(rows.length, 1);
  assertEquals(rows[0].tagmaId, B);
});

Deno.test(
  "when the min side starts reporting it replaces the max-side row",
  async () => {
    const h = new Harness();
    h.script.set(B, [entry(B, A)]);
    await h.refreshTagma(B, 1);
    h.script.set(A, [entry(A, B, "stamped-by-min")]);
    await h.refreshTagma(A, 2);
    const rows = h.list();
    assertEquals(rows.length, 1);
    assertEquals(rows[0].tagmaId, A);
    assertEquals(rows[0].peerHandle, "stamped-by-min");
  },
);

Deno.test("a pair unseen for the prune window ages out", async () => {
  const h = new Harness();
  h.mergeForTest([entry(A, B)], 1);
  assertEquals(h.list().length, 1);
  // The store's own tick counter drives the window (stamp 1, PRUNE_TICKS 5):
  // four sweeps keep it, the fifth is the last fresh one, the sixth prunes.
  await h.tick();
  await h.tick();
  await h.tick();
  await h.tick();
  assertEquals(h.list().length, 1);
  await h.tick();
  await h.tick();
  assertEquals(h.list().length, 0);
});

Deno.test("a dead daemon's fetch error is swallowed by the sweep", async () => {
  const h = new Harness();
  h.script.set(A, new Error("dead"));
  await h.refreshTagma(A, 1);
  assertEquals(h.list().length, 0);
});

Deno.test("peerLabel falls back to handle then the fallback label", () => {
  const h = new Harness();
  assertEquals(h.peerLabel(B, "b@owner"), "b@owner");
  assertEquals(h.peerLabel(B, ""), `Session ${B.slice(0, 8)}`);
});

Deno.test(
  "fetchTranscript halves the limit on 502 and never repeats params",
  async () => {
    const h = new Harness();
    h.manageScript = [
      { status: 502 },
      { status: 502 },
      {
        status: 200,
        body: [{ seq: 1, sender: {}, text: "hi", created_at: "" }],
      },
    ];
    const rows = await h.fetchTranscript(A, B, 0);
    const limits = h.calls.map(
      (p) => new URL("http://x/" + p).searchParams.get("limit") ?? "",
    );
    assertEquals(limits, ["25", "12", "6"]);
    assertEquals(rows.length, 1);
  },
);

Deno.test(
  "fetchTranscript gives up at the one-row floor after repeated 502s",
  async () => {
    const h = new Harness();
    h.manageScript = Array.from({ length: 10 }, () => ({ status: 502 }));
    await assertRejects(() => h.fetchTranscript(A, B, 0));
    const limits = h.calls.map(
      (p) => new URL("http://x/" + p).searchParams.get("limit") ?? "",
    );
    assertEquals(limits[0], "25");
    assertEquals(limits[limits.length - 1], "1");
    assertEquals(new Set(limits).size, limits.length);
  },
);
