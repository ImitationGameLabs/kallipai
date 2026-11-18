// Tests for TagmaClient over the shared seam: the SSE subscription's liveness
// wiring, and the rules every verb rides -- the bearer and credential mode the
// direct path sends, the two error classes, and the envelope's dangling list
// surviving the crossing as data.

import { assertEquals } from "@std/assert";
import {
  KallipaiError,
  TransportError,
  UNREADABLE_BODY_MESSAGE,
} from "@kallipai/kallipai-common";
import { withFetch } from "@kallipai/kallipai-common/testing";
import { TagmaClient } from "./client.ts";
import type { ProfileConfigPutRequest } from "./types.ts";

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

/** Answer every request with one fixed response. */
function answering(response: () => Response) {
  return () => Promise.resolve(response());
}

Deno.test(
  "externalEventStream feeds the liveness callback from keepalive comments",
  async () => {
    let liveness = 0;
    const events: { readonly event: string }[] = [];
    await withFetch(
      answering(() => sseResponse([":\n\n", ":\n\n", ":\n\n"])),
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
    await withFetch(
      answering(() =>
        sseResponse([":\n\n", 'event: status\ndata: {"n":1}\n\n']),
      ),
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

Deno.test("a verb sends the bearer and no cookie", async () => {
  const calls: Array<{ url: string; init?: RequestInit }> = [];
  await withFetch(
    (url, init) => {
      calls.push({ url: String(url), init });
      return Promise.resolve(
        new Response(
          JSON.stringify({
            budget: 0,
            consumed: 0,
            remaining: 0,
            unlimited: true,
          }),
          { status: 200 },
        ),
      );
    },
    async () => {
      const client = new TagmaClient({
        baseUrl: "http://tagma.test/",
        authToken: "t0ken",
      });
      await client.getBudget();
      assertEquals(calls.length, 1);
      const [call] = calls;
      assertEquals(call?.url, "http://tagma.test/budget");
      assertEquals(call?.init?.credentials, "omit");
      const headers = call?.init?.headers as Record<string, string>;
      assertEquals(headers.Authorization, "Bearer t0ken");
    },
  );
});

Deno.test("a refusal arrives as KallipaiError with its status", async () => {
  await withFetch(
    answering(
      () =>
        new Response(JSON.stringify({ error: { message: "no token" } }), {
          status: 401,
        }),
    ),
    async () => {
      const client = new TagmaClient({ baseUrl: "http://tagma.test" });
      const caught = await client.getRootAgent().catch((e: unknown) => e);
      assertEquals(caught instanceof KallipaiError, true);
      const err = caught as KallipaiError;
      assertEquals(err.api.status, 401);
      assertEquals(err.api.message, "no token");
    },
  );
});

Deno.test("a 409 keeps its dangling list as data", async () => {
  await withFetch(
    answering(
      () =>
        new Response(
          JSON.stringify({
            error: {
              message: "still bound",
              dangling: ["agent-x"],
              code: "dangling_bindings",
            },
          }),
          { status: 409 },
        ),
    ),
    async () => {
      const client = new TagmaClient({ baseUrl: "http://tagma.test" });
      const caught = await client.putTimezone("UTC").catch((e: unknown) => e);
      assertEquals(caught instanceof KallipaiError, true);
      const err = caught as KallipaiError;
      assertEquals(err.api.status, 409);
      assertEquals(err.api.dangling, ["agent-x"]);
      assertEquals(err.api.code, "dangling_bindings");
    },
  );
});

Deno.test("a 204 answer resolves to undefined", async () => {
  await withFetch(
    answering(() => new Response(null, { status: 204 })),
    async () => {
      const client = new TagmaClient({ baseUrl: "http://tagma.test" });
      const answer = await client.setAgentDuty("root", { status: "offduty" });
      assertEquals(answer, undefined);
    },
  );
});

Deno.test("a status-line verb reads no body", async () => {
  await withFetch(
    answering(() => new Response("interrupted", { status: 200 })),
    async () => {
      const client = new TagmaClient({ baseUrl: "http://tagma.test" });
      const answer = await client.interruptAgent("root");
      assertEquals(answer, undefined);
    },
  );
});

Deno.test("a dead exchange arrives as TransportError", async () => {
  await withFetch(
    () => Promise.reject(new TypeError("connection refused")),
    async () => {
      const client = new TagmaClient({ baseUrl: "http://tagma.test" });
      const caught = await client.getBudget().catch((e: unknown) => e);
      assertEquals(caught instanceof TransportError, true);
      assertEquals((caught as TransportError).message, "connection refused");
    },
  );
});

Deno.test("the stream's own refusal arrives as KallipaiError", async () => {
  await withFetch(
    answering(
      () =>
        new Response(JSON.stringify({ error: { message: "no such agent" } }), {
          status: 404,
        }),
    ),
    async () => {
      const client = new TagmaClient({ baseUrl: "http://tagma.test" });
      const stream = client.externalEventStream("root");
      const caught = await stream.next().catch((e: unknown) => e);
      assertEquals(caught instanceof KallipaiError, true);
      assertEquals((caught as KallipaiError).api.status, 404);
    },
  );
});

/** Record every `AbortSignal.timeout` delay the seam asks for, then forward. */
async function recordingWindows<T>(
  run: () => Promise<T>,
): Promise<{ readonly delays: number[]; readonly result: T }> {
  const delays: number[] = [];
  const real = AbortSignal.timeout;
  AbortSignal.timeout = (ms = 0) => {
    delays.push(ms);
    return real(ms);
  };
  try {
    return { delays, result: await run() };
  } finally {
    AbortSignal.timeout = real;
  }
}

Deno.test("a JSON verb asks the seam for the 30s window", async () => {
  const { delays } = await recordingWindows(() =>
    withFetch(
      answering(() => new Response("{}", { status: 200 })),
      async () => {
        const client = new TagmaClient({ baseUrl: "http://tagma.test" });
        await client.getProfiles();
      },
    ),
  );
  assertEquals(delays, [30_000]);
});

Deno.test("each profile route asks the seam for its own window", async () => {
  const { delays } = await recordingWindows(() =>
    withFetch(
      answering(() => new Response("{}", { status: 200 })),
      async () => {
        const client = new TagmaClient({ baseUrl: "http://tagma.test" });
        await client.updateProfiles({} as ProfileConfigPutRequest); // pull
        await client.refreshProfiles(); // pull
        await client.probeProfiles({ endpoints: [], sets: [] }); // probe
      },
    ),
  );
  assertEquals(delays, [150_000, 150_000, 90_000]);
});

Deno.test("the archive download asks the seam for no window", async () => {
  const bytes = new Uint8Array([1, 2, 3]);
  const { delays, result } = await recordingWindows(() =>
    withFetch(
      answering(() => new Response(bytes, { status: 200 })),
      () =>
        new TagmaClient({ baseUrl: "http://tagma.test" }).fetchTaskArchive(7),
    ),
  );
  assertEquals(delays, []);
  assertEquals(result, bytes);
});

Deno.test("an unreadable 2xx body arrives as TransportError", async () => {
  const broken = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.error(new TypeError("the body broke"));
    },
  });
  await withFetch(
    answering(() => new Response(broken, { status: 200 })),
    async () => {
      const client = new TagmaClient({ baseUrl: "http://tagma.test" });
      const caught = await client.fetchTaskArchive(7).catch((e: unknown) => e);
      assertEquals(caught instanceof TransportError, true);
      assertEquals((caught as TransportError).message, UNREADABLE_BODY_MESSAGE);
    },
  );
});
