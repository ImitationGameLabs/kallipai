// Projection client tests: the three cached GETs, the dirty-frame SSE
// generator, and the linear backoff. The transport is a stubbed global fetch
// (same seam the browser uses), so the assertions cover the exact paths and
// the wire shapes the lesche serves.

import { assertEquals, assertRejects } from "@std/assert";
import { LinearBackoff, ProjectionClient } from "./projection.ts";

const AGENTS_BODY = {
  stale: false,
  seq: 7,
  updated_at: 1_700_000_000,
  agents: [{ id: "root", state: "idle" }],
  status: { root_state: "idle" },
};

function withFetch(
  stub: (url: string, init?: RequestInit) => Promise<Response>,
) {
  const real = globalThis.fetch;
  globalThis.fetch = stub as typeof fetch;
  return () => {
    globalThis.fetch = real;
  };
}

Deno.test("projection agents GET hits the per-tagma agents path", async () => {
  const seen: string[] = [];
  const restore = withFetch((url) => {
    seen.push(url);
    return Promise.resolve(Response.json(AGENTS_BODY));
  });
  try {
    const client = new ProjectionClient("http://lesche.test/v1/lesche");
    const resp = await client.agents("t-a");
    assertEquals(seen, ["http://lesche.test/v1/lesche/tagmata/t-a/agents"]);
    assertEquals(resp.seq, 7);
    assertEquals(resp.stale, false);
    assertEquals(resp.agents.length, 1);
  } finally {
    restore();
  }
});

Deno.test("projection GET failure surfaces the status", async () => {
  const restore = withFetch(() =>
    Promise.resolve(new Response("no projection", { status: 404 })),
  );
  try {
    const client = new ProjectionClient("http://lesche.test/v1/lesche");
    await assertRejects(() => client.agents("gone"), Error, "404");
  } finally {
    restore();
  }
});

Deno.test(
  "projection events generator yields dirty frames from the sse stream",
  async () => {
    const sseBody =
      'data: {"tagma_id":"t-a","seq":1}\n\n' +
      'data: {"tagma_id":"t-a","seq":2}\n\n';
    const restore = withFetch((url) => {
      assertEquals(url, "http://lesche.test/v1/lesche/tagmata/t-a/state");
      return Promise.resolve(
        new Response(sseBody, {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        }),
      );
    });
    try {
      const client = new ProjectionClient("http://lesche.test/v1/lesche");
      const frames: number[] = [];
      for await (const dirty of client.state("t-a")) {
        frames.push(dirty.seq);
        if (frames.length === 2) break;
      }
      assertEquals(frames, [1, 2]);
    } finally {
      restore();
    }
  },
);

Deno.test("linear backoff steps by 2s and caps at 30s", () => {
  const backoff = new LinearBackoff();
  assertEquals(backoff.next(), 0);
  assertEquals(backoff.next(), 2_000);
  assertEquals(backoff.next(), 4_000);
  backoff.reset();
  assertEquals(backoff.next(), 0);
  const hot = new LinearBackoff(2_000, 6_000);
  hot.next();
  hot.next();
  hot.next();
  hot.next();
  hot.next();
  assertEquals(hot.next(), 6_000);
});
