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
      assertEquals(await store.saveSet("s1", { description: "x" }), false);
      assertEquals(store.error !== null, true);
      assertEquals(await store.removeSet("s1"), false);
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);
