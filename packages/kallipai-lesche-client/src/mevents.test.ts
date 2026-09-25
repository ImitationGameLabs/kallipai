// meEvents shaping: a canned SSE body is parsed (via the shared @kallipai/
// kallipai-common parseSseStream) into MeEventFrames. This validates the fetch +
// SSE wiring; it does not re-test SSE framing itself.

import { assertEquals } from "@std/assert";
import { LescheClient } from "./http.ts";

Deno.test(
  "meEvents yields id-less payload frames without a cursor",
  async () => {
    const envelope = {
      channel_id: "c1",
      sender: { id: "p-tagma-1", kind: "agent", handle: "Tagma" },
      sequence_n: 0,
      trace_id: "tr",
      timestamp: "2024-01-01T00:00:00.000Z",
      ciphertext: "AAAA",
    };
    const body =
      `data: ${JSON.stringify({ type: "envelope", envelope })}\n\n` +
      `: keepalive\n\n` +
      `data: ${JSON.stringify({ type: "tagma_online", tagma_id: "t2" })}\n\n`;

    const originalFetch = globalThis.fetch;
    globalThis.fetch = (() =>
      Promise.resolve(
        new Response(body, {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
      )) as typeof fetch;
    try {
      const client = new LescheClient("http://x");
      const events = [];
      for await (const ev of client.meEvents()) {
        events.push(ev);
      }
      assertEquals(events.length, 2);
      const first = events[0]!;
      const second = events[1]!;
      // No `id:` and no `event:` field on either frame: the unversioned
      // degradation shape (an older server).
      if (first.kind !== "event" || second.kind !== "event") {
        throw new Error("expected event frames");
      }
      assertEquals(first.epoch, null);
      assertEquals(first.seq, null);
      assertEquals(first.event.type, "envelope");
      if (first.event.type !== "envelope") throw new Error("unreachable");
      assertEquals(first.event.envelope.channel_id, "c1");
      assertEquals(second.event.type, "tagma_online");
      if (second.event.type !== "tagma_online") throw new Error("unreachable");
      assertEquals(second.event.tagma_id, "t2");
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);

Deno.test("meEvents passes the marker and id cursors through", async () => {
  const body =
    `event: stream\nid: 0\ndata: ${JSON.stringify({ epoch: 3, next_seq: 0 })}\n\n` +
    `id: 3:0\nevent: tagma_online\ndata: ${JSON.stringify({ type: "tagma_online", tagma_id: "t2" })}\n\n`;

  const originalFetch = globalThis.fetch;
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(body, {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      }),
    )) as typeof fetch;
  try {
    const client = new LescheClient("http://x");
    const events = [];
    for await (const ev of client.meEvents()) {
      events.push(ev);
    }
    assertEquals(events.length, 2);
    assertEquals(events[0], { kind: "stream", epoch: 3, nextSeq: 0 });
    assertEquals(events[1], {
      kind: "event",
      epoch: 3,
      seq: 0,
      event: { type: "tagma_online", tagma_id: "t2" },
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test("meEvents parses a zero epoch out of the id", async () => {
  const body = `id: 0:7\nevent: tagma_online\ndata: ${JSON.stringify({ type: "tagma_online", tagma_id: "t2" })}\n\n`;

  const originalFetch = globalThis.fetch;
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(body, {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      }),
    )) as typeof fetch;
  try {
    const client = new LescheClient("http://x");
    const events = [];
    for await (const ev of client.meEvents()) {
      events.push(ev);
    }
    // Epoch 0 is a real epoch: a truthy id parse would null it out.
    assertEquals(events, [
      {
        kind: "event",
        epoch: 0,
        seq: 7,
        event: { type: "tagma_online", tagma_id: "t2" },
      },
    ]);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test("meEvents marker precedes the payload frames", async () => {
  // Same wire shape as the passthrough leg, with the id dropped from the
  // payload: ordering (marker first) holds independently of cursoring.
  const body =
    `event: stream\ndata: ${JSON.stringify({ epoch: 1, next_seq: 7 })}\n\n` +
    `data: ${JSON.stringify({ type: "tagma_offline", tagma_id: "t9" })}\n\n`;

  const originalFetch = globalThis.fetch;
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(body, {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      }),
    )) as typeof fetch;
  try {
    const client = new LescheClient("http://x");
    const events = [];
    for await (const ev of client.meEvents()) {
      events.push(ev);
    }
    assertEquals(events.length, 2);
    assertEquals(events[0], { kind: "stream", epoch: 1, nextSeq: 7 });
    assertEquals(events[1], {
      kind: "event",
      epoch: null,
      seq: null,
      event: { type: "tagma_offline", tagma_id: "t9" },
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test("meEvents surfaces a non-2xx response as an error", async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(JSON.stringify({ error: { message: "unauthorized" } }), {
        status: 401,
      }),
    )) as typeof fetch;
  try {
    const client = new LescheClient("http://x");
    const iter = client.meEvents();
    let threw = false;
    try {
      await iter.next();
    } catch (e) {
      threw = true;
      assertEquals((e as { status: number }).status, 401);
    }
    if (!threw) throw new Error("expected meEvents to throw on 401");
  } finally {
    globalThis.fetch = originalFetch;
  }
});
