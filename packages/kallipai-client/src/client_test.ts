// Tests for TagmaClient's SSE subscription, focused on the seam between the
// shared parser and the caller's liveness callback: the tagma's keepalive
// comments carry no event, so liveness has to come from raw bytes arriving.

import { assertEquals } from "@std/assert";
import { TagmaClient } from "./client.ts";

/** A fetch response whose body is the given chunks, then EOF. */
function sseResponse(chunks: string[]): Response {
  const encoder = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    },
  });
  return new Response(body, {
    status: 200,
    headers: { "content-type": "text/event-stream" },
  });
}

/** Run `fn` with `fetch` replaced by a stub returning `response`. */
async function withStubFetch<T>(
  response: () => Response,
  fn: () => Promise<T>,
): Promise<T> {
  const original = globalThis.fetch;
  globalThis.fetch = (() => Promise.resolve(response())) as typeof fetch;
  try {
    return await fn();
  } finally {
    globalThis.fetch = original;
  }
}

Deno.test(
  "externalEventStream feeds the liveness callback from keepalive comments",
  async () => {
    let liveness = 0;
    const events: { readonly event: string }[] = [];
    await withStubFetch(
      () => sseResponse([":\n\n", ":\n\n", ":\n\n"]),
      async () => {
        const client = new TagmaClient({ baseUrl: "http://tagma.test" });
        for await (const frame of client.externalEventStream(
          "root",
          undefined,
          () => {
            liveness++;
          },
        )) {
          events.push(frame);
        }
      },
    );
    // Comment blocks carry no event name, so they are never yielded...
    assertEquals(events, []);
    // ...but they are bytes on the wire: one report at open plus one per
    // comment block. Without this the 45s watchdog fires on a live stream.
    assertEquals(liveness, 4);
  },
);

Deno.test(
  "externalEventStream yields data frames and still reports liveness",
  async () => {
    let liveness = 0;
    const events: { readonly event: string; readonly data: string }[] = [];
    await withStubFetch(
      () => sseResponse([":\n\n", 'event: status\ndata: {"n":1}\n\n']),
      async () => {
        const client = new TagmaClient({ baseUrl: "http://tagma.test" });
        for await (const frame of client.externalEventStream(
          "root",
          undefined,
          () => {
            liveness++;
          },
        )) {
          events.push(frame);
        }
      },
    );
    assertEquals(events, [{ event: "status", data: '{"n":1}' }]);
    assertEquals(liveness, 3);
  },
);
