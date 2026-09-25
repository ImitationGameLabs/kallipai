// Cursor tracking: unit legs for the MeCursor judge table and
// one integration leg driving a real LescheClient.meEvents over a canned SSE
// body through the cursor and the resync batcher.

import { assertEquals } from "@std/assert";
import type { MeEventFrame } from "@kallipai/kallipai-lesche-client";
import { LescheClient } from "@kallipai/kallipai-lesche-client";
import { MeCursor, ResyncBatcher } from "./cursor.ts";
import type { ResyncPlan } from "./cursor.ts";

function stream(epoch: number, nextSeq: number): MeEventFrame {
  return { kind: "stream", epoch, nextSeq };
}

function event(epoch: number | null, seq: number | null): MeEventFrame {
  return {
    kind: "event",
    epoch,
    seq,
    event: { type: "tagma_online", tagma_id: "t1" },
  };
}

Deno.test("cursor: first marker anchors the cursor with no plan", () => {
  const c = new MeCursor();
  assertEquals(c.observe(stream(3, 0)), null);
  assertEquals(c.observe(event(3, 0)), null);
  assertEquals(c.observe(event(3, 1)), null);
});

Deno.test("cursor: seamless continuation yields no plan", () => {
  const c = new MeCursor();
  c.observe(stream(3, 0));
  for (const seq of [0, 1, 2, 3]) {
    assertEquals(c.observe(event(3, seq)), null);
  }
});

Deno.test("cursor: a skipped seq is a gap and advances past the hole", () => {
  const c = new MeCursor();
  c.observe(stream(3, 0));
  c.observe(event(3, 0));
  c.observe(event(3, 1));
  // seq 2 lost: next arrival is 4.
  assertEquals(c.observe(event(3, 4)), { kind: "gap" });
  // The cursor now expects 5; the following frame is in-order again.
  assertEquals(c.observe(event(3, 5)), null);
});

Deno.test("cursor: an epoch change in the marker is a full resync", () => {
  const c = new MeCursor();
  c.observe(stream(3, 0));
  c.observe(event(3, 0));
  c.observe(event(3, 1));
  assertEquals(c.observe(stream(9, 0)), { kind: "full" });
  // The new epoch's frames are judged fresh: seq 0 is not "behind".
  assertEquals(c.observe(event(9, 0)), null);
});

Deno.test(
  "cursor: an epoch change resets the floor, so post-restart gaps are detected",
  () => {
    const c = new MeCursor();
    c.observe(stream(3, 0));
    c.observe(event(3, 0));
    c.observe(event(3, 1)); // the old epoch's floor is 2
    assertEquals(c.observe(stream(9, 0)), { kind: "full" });
    // The new stream reallocates from zero: seq 0 is in-order, not behind.
    assertEquals(c.observe(event(9, 0)), null);
    // And the tracker is live again: losing seq 1 is a gap, not a frame
    // quietly swallowed by the old epoch's floor.
    assertEquals(c.observe(event(9, 2)), { kind: "gap" });
  },
);

Deno.test(
  "cursor: same-epoch reconnect marker detects the disconnect window",
  () => {
    const c = new MeCursor();
    c.observe(stream(3, 0));
    c.observe(event(3, 0));
    c.observe(event(3, 1));
    c.observe(event(3, 2)); // expected is now 3
    // Reconnect to the same (still-running) lesche: the marker's next_seq
    // proves seqs 3-4 were allocated (and lost) while we were away.
    assertEquals(c.observe(stream(3, 5)), { kind: "gap" });
    assertEquals(c.observe(event(3, 5)), null);
  },
);

Deno.test("cursor: expectedSeq never rewinds (monotonicity clamp)", () => {
  const c = new MeCursor();
  c.observe(stream(3, 0));
  c.observe(event(3, 0));
  c.observe(event(3, 4)); // gap to expected 5
  // An anomalous smaller next_seq must not pull the cursor backwards.
  assertEquals(c.observe(stream(3, 2)), null);
  // seq 4 was already consumed, so it is a duplicate now, not a gap.
  assertEquals(c.observe(event(3, 4)), null);
  assertEquals(c.observe(event(3, 5)), null);
});

Deno.test(
  "cursor: id-less frames degrade to unversioned, sticky until a marker",
  () => {
    const c = new MeCursor();
    assertEquals(c.observe(event(null, null)), null);
    // Sticky: even a jumped seq cannot be judged without a cursor.
    assertEquals(c.observe(event(3, 99)), null);
    // A marker proves the server is versioned again: tracking re-arms.
    assertEquals(c.observe(stream(3, 0)), null);
    assertEquals(c.observe(event(3, 1)), { kind: "gap" });
  },
);

Deno.test("cursor: duplicates and pre-adoption frames are tolerated", () => {
  const c = new MeCursor();
  c.observe(stream(3, 2));
  // Pre-adoption: allocated before the marker capture, arrives after it.
  assertEquals(c.observe(event(3, 0)), null);
  assertEquals(c.observe(event(3, 1)), null);
  assertEquals(c.observe(event(3, 2)), null);
  // A literal duplicate is also silent.
  assertEquals(c.observe(event(3, 2)), null);
  assertEquals(c.observe(event(3, 3)), null);
});

Deno.test(
  "resync: a burst of loss coalesces into exactly one gap plan",
  async () => {
    const envelope = (seq: number) =>
      `id: 3:${seq}\nevent: envelope\ndata: ${JSON.stringify({ type: "envelope", envelope: { channel_id: "c1", sender: { id: "p1", kind: "agent", handle: "T" }, sequence_n: seq, trace_id: "tr", timestamp: "2024-01-01T00:00:00.000Z", ciphertext: "AAAA" } })}\n\n`;
    const body =
      `event: stream\ndata: ${JSON.stringify({ epoch: 3, next_seq: 0 })}\n\n` +
      [0, 1, 2, 3, 4].map(envelope).join("") +
      // seqs 5 and 6 are lost; the stream continues at 7.
      [7, 8, 9].map(envelope).join("");

    const originalFetch = globalThis.fetch;
    globalThis.fetch = (() =>
      Promise.resolve(
        new Response(body, {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
      )) as typeof fetch;
    try {
      const plans: ResyncPlan[] = [];
      const batcher = new ResyncBatcher((plan) => plans.push(plan), 1);
      const cursor = new MeCursor();
      const client = new LescheClient("http://x");
      for await (const frame of client.meEvents()) {
        const plan = cursor.observe(frame);
        if (plan) batcher.push(plan);
      }
      // One trailing-edge timer, one fire — the seven individual judgements
      // (five in-order, one gap at 7, two in-order) cost a single refetch.
      await new Promise((r) => setTimeout(r, 20));
      assertEquals(plans.length, 1);
      assertEquals(plans[0], { kind: "gap" });
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);
