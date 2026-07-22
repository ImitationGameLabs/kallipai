// meEvents shaping: a canned SSE body is parsed (via the shared @kallipai/
// kallip-common parseSseStream) into AgoraEvents. This validates the fetch +
// SSE wiring; it does not re-test SSE framing itself.

import { assertEquals } from "@std/assert";
import { LescheClient } from "./http.ts";

Deno.test("meEvents parses an SSE stream into AgoraEvents", async () => {
  const envelope = {
    conversation_id: "c1",
    sender: { kind: "agent", tagma_id: "t1" },
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
    if (first.type !== "envelope") throw new Error("expected envelope");
    assertEquals(first.envelope.conversation_id, "c1");
    if (second.type !== "tagma_online")
      throw new Error("expected tagma_online");
    assertEquals(second.tagma_id, "t2");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test("meEvents surfaces a non-2xx response as an error", async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(JSON.stringify({ message: "unauthorized" }), {
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
