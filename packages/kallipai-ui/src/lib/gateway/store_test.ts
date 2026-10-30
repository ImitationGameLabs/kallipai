// Behavior tests for the gateway store's failure semantics: run() answers
// undefined on failure (the caller-visible marker behind the boolean
// returns) and refreshAll() merges the seven reads independently -- a
// failed slice keeps its previously read value while the rest land.
//
// The store module is rune-bearing, but deno test runs it uncompiled, so a
// passthrough $state shim lets it run with plain fields (the
// channels_refresh_test pattern). Assertions face the observable store
// state, never the fetch call sequence.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const store = (await import("./store.svelte.ts")).userGatewayStore;

/** Stub fetch with per-path status/body answers; unmatched paths fail. */
function scriptedFetch(
  routes: Record<string, { status?: number; body?: unknown }>,
): typeof fetch {
  const fake = ((input: unknown) => {
    const hit = routes[String(input)] ?? { status: 500 };
    const status = hit.status ?? 200;
    const stub = {
      ok: status >= 200 && status < 300,
      status,
      json: () => Promise.resolve(hit.body ?? {}),
    } as unknown as Response;
    return Promise.resolve(stub);
  }) as unknown as typeof fetch;
  return fake;
}

const OK = { status: 200, body: {} };

Deno.test(
  "refreshAll keeps a failed slice's previous value and records the error",
  async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = scriptedFetch({
      "/user/providers": {
        body: {
          providers: [
            {
              provider_id: "p1",
              family: "f",
              base_url: null,
              api_key_masked: "m",
            },
          ],
        },
      },
      "/user/profiles": OK,
      "/user/sets": OK,
      "/user/collections": OK,
      "/user/groups": OK,
      "/user/platform-collections": OK,
    });
    try {
      await store.refreshAll();
    } finally {
      globalThis.fetch = originalFetch;
    }
    assertEquals(store.providers?.length, 1);

    globalThis.fetch = scriptedFetch({
      "/user/profiles": OK,
      "/user/sets": OK,
      "/user/collections": OK,
      "/user/groups": OK,
      "/user/platform-collections": OK,
    });
    try {
      await store.refreshAll();
      // The failed slice keeps the previously read value.
      assertEquals(store.providers?.length, 1);
      assertEquals(store.error !== null, true);
      assertEquals(store.hasLoaded, true);
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);

Deno.test(
  "a failed mutation answers false and surfaces the store error",
  async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = scriptedFetch({});
    try {
      assertEquals(await store.patchDescription("c1", "s1", "x"), false);
      assertEquals(store.error !== null, true);
      assertEquals(await store.removeSet("c1", "s1"), false);
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);

Deno.test("a set save writes and does not read", async () => {
  const originalFetch = globalThis.fetch;
  const seen: string[] = [];
  // Two writes and no read at all: the read side belongs to the save
  // sequence, so a store write that refreshed behind its own back would
  // double the reads the sequence already paid for.
  globalThis.fetch = ((input: unknown, init?: { method?: string }) => {
    seen.push(`${init?.method ?? "GET"} ${String(input)}`);
    return Promise.resolve(
      new Response(
        JSON.stringify({ name: "s1", description: "x", members: [] }),
        { status: 200, headers: { "content-type": "application/json" } },
      ),
    );
  }) as unknown as typeof fetch;
  try {
    assertEquals(await store.patchDescription("c1", "s1", "x"), true);
    assertEquals(await store.reorderSet("c1", "s1", ["p1", "p2"]), true);
    assertEquals(seen.length, 2);
    assertEquals(seen[0]?.startsWith("PATCH "), true);
    assertEquals(seen[0]?.endsWith("/user/collections/c1/sets/s1"), true);
    assertEquals(seen[1]?.startsWith("PUT "), true);
    assertEquals(seen[1]?.endsWith("/user/collections/c1/sets/s1/order"), true);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test("a PATCH that answers 204 still counts as saved", async () => {
  const originalFetch = globalThis.fetch;
  // The user route answers 204 with no body: the write landed, and the
  // store answers from whether the request threw, not from what came back.
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(null, { status: 204 }),
    )) as unknown as typeof fetch;
  try {
    assertEquals(await store.patchDescription("c1", "s1", "x"), true);
    assertEquals(store.error, null);
  } finally {
    globalThis.fetch = originalFetch;
  }
});
