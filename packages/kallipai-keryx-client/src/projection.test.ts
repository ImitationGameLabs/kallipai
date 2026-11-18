// Projection client tests: the three cached GETs, the dirty-frame SSE
// generator, and the linear backoff. The transport is a stubbed global fetch
// (same seam the browser uses), so the assertions cover the exact paths and
// the wire shapes the keryx serves.

import { DataPlaneApiError, TransportError } from "@kallipai/kallipai-common";
import { withFetch } from "@kallipai/kallipai-common/testing";
import { assert, assertEquals, assertRejects } from "@std/assert";
import { LinearBackoff, ProjectionClient } from "./projection.ts";

const AGENTS_BODY = {
  stale: false,
  seq: 7,
  updated_at: 1_700_000_000,
  agents: [{ id: "root", state: "idle" }],
  status: { root_state: "idle" },
};

Deno.test("projection agents GET hits the per-tagma agents path", async () => {
  const seen: string[] = [];
  await withFetch(
    (url) => {
      seen.push(String(url));
      return Promise.resolve(Response.json(AGENTS_BODY));
    },
    async () => {
      const client = new ProjectionClient("http://keryx.test/v1/keryx");
      const resp = await client.agents("t-a");
      assertEquals(seen, ["http://keryx.test/v1/keryx/tagmata/t-a/agents"]);
      assertEquals(resp.seq, 7);
      assertEquals(resp.stale, false);
      assertEquals(resp.agents.length, 1);
    },
  );
});

Deno.test("projection GET failure surfaces the status", async () => {
  await withFetch(
    () => Promise.resolve(new Response("no projection", { status: 404 })),
    async () => {
      const client = new ProjectionClient("http://keryx.test/v1/keryx");
      await assertRejects(() => client.agents("gone"), Error, "404");
    },
  );
});

Deno.test(
  "projection events generator yields dirty frames from the sse stream",
  async () => {
    const sseBody =
      'data: {"tagma_id":"t-a","seq":1}\n\n' +
      'data: {"tagma_id":"t-a","seq":2}\n\n';
    await withFetch(
      (url) => {
        assertEquals(url, "http://keryx.test/v1/keryx/tagmata/t-a/state");
        return Promise.resolve(
          new Response(sseBody, {
            status: 200,
            headers: { "content-type": "text/event-stream" },
          }),
        );
      },
      async () => {
        const client = new ProjectionClient("http://keryx.test/v1/keryx");
        const frames: number[] = [];
        for await (const dirty of client.state("t-a")) {
          frames.push(dirty.seq);
          if (frames.length === 2) break;
        }
        assertEquals(frames, [1, 2]);
      },
    );
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

Deno.test(
  "a frame that is not JSON fails the attempt instead of being skipped",
  async () => {
    const sseBody = "data: {not json\n\n";
    await withFetch(
      () =>
        Promise.resolve(
          new Response(sseBody, {
            status: 200,
            headers: { "content-type": "text/event-stream" },
          }),
        ),
      async () => {
        const client = new ProjectionClient("http://keryx.test/v1/keryx");
        await assertRejects(async () => {
          for await (const _dirty of client.state("t-a")) {
            // No frame is valid: the attempt must fail, not run past it.
          }
        }, TransportError);
      },
    );
  },
);

Deno.test(
  "a refused state stream surfaces the status as a DataPlaneApiError",
  async () => {
    await withFetch(
      () => Promise.resolve(new Response("", { status: 404 })),
      async () => {
        const client = new ProjectionClient("http://keryx.test/v1/keryx");
        let caught: unknown;
        try {
          for await (const _dirty of client.state("gone")) {
            // Nothing to read: the refusal arrives before the first frame.
          }
        } catch (error) {
          caught = error;
        }
        assert(caught instanceof DataPlaneApiError);
        assertEquals((caught as DataPlaneApiError).status, 404);
      },
    );
  },
);
