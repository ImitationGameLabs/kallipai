// keryxEvents shaping: a canned SSE body is parsed (via the shared
// @kallipai/kallipai-common frame machine) into KeryxEventFrames. This
// validates the keryx client's fetch + SSE wiring (the frame machine itself
// is shared with the lesche client and pinned by the lesche mevents tests);
// the legs here mirror the lesche twins for the bilateral family.

import { assert, assertEquals } from "@std/assert";
import type { KeryxEvent } from "./types.ts";
import { KeryxClient } from "./http.ts";
import { DataPlaneApiError } from "./types.ts";

Deno.test("keryxEvents passes the marker and id cursors through", async () => {
  const body =
    `event: stream\nid: 0\ndata: ${JSON.stringify({ epoch: 3, next_seq: 0 })}\n\n` +
    `id: 3:1\nevent: tagma_online\ndata: ${JSON.stringify({ type: "tagma_online", tagma_id: "t2" })}\n\n`;

  const originalFetch = globalThis.fetch;
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(body, {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      }),
    )) as typeof fetch;
  try {
    const client = new KeryxClient("http://x");
    const events = [];
    for await (const ev of client.keryxEvents()) {
      events.push(ev);
    }
    assertEquals(events.length, 2);
    assertEquals(events[0], { kind: "stream", epoch: 3, nextSeq: 0 });
    assertEquals(events[1], {
      kind: "event",
      epoch: 3,
      seq: 1,
      event: { type: "tagma_online", tagma_id: "t2" },
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test(
  "keryxEvents yields id-less bilateral frames without a cursor",
  async () => {
    const fileDelivered: KeryxEvent = {
      type: "file_delivered",
      record_id: "r1",
      path: "inbox/x.bin",
      from: "@peer",
      name: "x.bin",
      size: 12,
    };
    const body = `data: ${JSON.stringify(fileDelivered)}\n\n`;

    const originalFetch = globalThis.fetch;
    globalThis.fetch = (() =>
      Promise.resolve(
        new Response(body, {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
      )) as typeof fetch;
    try {
      const client = new KeryxClient("http://x");
      const events = [];
      for await (const ev of client.keryxEvents()) {
        events.push(ev);
      }
      assertEquals(events.length, 1);
      const first = events[0]!;
      // No `id:` and no `event:` field: the unversioned degradation shape (an
      // older server).
      if (first.kind !== "event") throw new Error("expected event frame");
      assertEquals(first.epoch, null);
      assertEquals(first.seq, null);
      assertEquals(first.event, fileDelivered);
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);

Deno.test("keryxEvents surfaces a non-2xx response as an error", async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(JSON.stringify({ error: { message: "unauthorized" } }), {
        status: 401,
      }),
    )) as typeof fetch;
  try {
    const client = new KeryxClient("http://x");
    const iter = client.keryxEvents();
    let threw = false;
    try {
      await iter.next();
    } catch (e) {
      threw = true;
      assert(e instanceof DataPlaneApiError);
      assertEquals((e as { status: number }).status, 401);
    }
    if (!threw) throw new Error("expected keryxEvents to throw on 401");
  } finally {
    globalThis.fetch = originalFetch;
  }
});
