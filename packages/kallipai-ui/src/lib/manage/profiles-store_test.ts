// Behavior test for the store's source-switch wiring: switchSource sends
// the empty profiles plus the source mode (the server reads only
// source.mode and ignores the rest by design), folds the response into
// both config and draft, and on refusal parks the error text while
// leaving the committed state untouched. The wire is faked at fetch, so
// the real client+store chain runs end to end.
//
// The store module is rune-bearing, but deno test runs it uncompiled,
// so a passthrough $state shim lets it run with plain fields (the
// gateway store_test pattern).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
  namespace $state {
    function snapshot<T>(v: T): T;
  }
}

(globalThis as Record<string, unknown>)["$state"] = Object.assign(
  (v: unknown) => v,
  {
    snapshot: (v: unknown) => v,
  },
);

const { assertEquals, assertRejects } = await import("@std/assert");
const mod = await import("./profiles.svelte.ts");
import { OfflineBackend } from "./backend.ts";
import { type ProfileConfig, TagmaClient } from "@kallipai/kallipai-client";

// A store pointed at a throwaway backend: the faked fetch answers every
// request, so the client only needs any well-formed base URL.
function wiredStore(): InstanceType<typeof mod.ProfilesStore> {
  const store = new mod.ProfilesStore();
  store.switchBackend(
    new OfflineBackend(
      new TagmaClient({
        baseUrl: "http://tagma.test",
        authToken: "t",
      }),
    ),
  );
  return store;
}

// A committed gateway-source config as the server answers a switch:
// the profile body plus the additive source block. The source_switch
// note the server also attaches is intentionally absent — the store
// treats the response as plain config.
const switchedConfig: ProfileConfig = {
  sets: {},
  default: "work",
  endpoints: {},
  parking: [],
  source: {
    mode: "model-gateway",
    polis: "https://polis.example",
    platforms: [],
    proxy_available: true,
    poisoned: false,
    token_state: "valid",
    last_refresh: null,
    refresh_failure_count: 0,
  },
};

Deno.test(
  "switchSource wires a source-only PUT and folds the response",
  async () => {
    const realFetch = globalThis.fetch;
    let seen: { method: string; body: unknown } | null = null;
    globalThis.fetch = ((_input: unknown, init?: RequestInit) => {
      seen = {
        method: String(init?.method ?? "GET"),
        body: JSON.parse(String(init?.body)),
      };
      return Promise.resolve(
        new Response(JSON.stringify(switchedConfig), {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
      );
    }) as typeof fetch;
    try {
      const store = wiredStore();
      await store.switchSource("model-gateway");
      // The request is the switch shape: the mode alone names
      // the change; the profile empties ride along unseen.
      assertEquals(seen!.method, "PUT");
      assertEquals(seen!.body, {
        sets: [],
        endpoints: {},
        parking: [],
        source: { mode: "model-gateway" },
      });
      // The response becomes both the committed config and the draft,
      // so the page re-renders in the new posture with no dirty delta.
      assertEquals(store.config, switchedConfig);
      assertEquals(store.draft, switchedConfig);
      assertEquals(store.isDirty, false);
      assertEquals(store.isSaving, false);
      assertEquals(store.error, null);
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test("switchSource parks the refusal and switches nothing", async () => {
  const realFetch = globalThis.fetch;
  globalThis.fetch = (() =>
    Promise.resolve(
      new Response(
        JSON.stringify({
          error: { message: "gateway unreachable: nothing was switched" },
        }),
        {
          status: 502,
          headers: { "content-type": "application/json" },
        },
      ),
    )) as typeof fetch;
  try {
    const store = wiredStore();
    await assertRejects(() => store.switchSource("model-gateway"));
    assertEquals(store.config, null);
    assertEquals(store.draft, null);
    assertEquals(store.isSaving, false);
    assertEquals(typeof store.error, "string");
  } finally {
    globalThis.fetch = realFetch;
  }
});

// A fetch mock that captures the request and answers with the given
// status and body (the switch/apply shape tests read the wire
// verbatim).
function captureFetch(
  status: number,
  body: unknown,
): { fetch: typeof fetch; seen: { method: string; body: unknown }[] } {
  const seen: { method: string; body: unknown }[] = [];
  const stub = ((_input: unknown, init?: RequestInit) => {
    seen.push({
      method: String(init?.method ?? "GET"),
      body: JSON.parse(String(init?.body)),
    });
    return Promise.resolve(
      new Response(JSON.stringify(body), {
        status,
        headers: { "content-type": "application/json" },
      }),
    );
  }) as typeof fetch;
  return { fetch: stub, seen };
}

Deno.test(
  "switchSource carries polis, a touched selection, and force on the wire",
  async () => {
    const realFetch = globalThis.fetch;
    const { fetch, seen } = captureFetch(200, switchedConfig);
    globalThis.fetch = fetch;
    try {
      const store = wiredStore();
      // The rebind shape: same mode as the live source would be, but
      // here just the wire presence that matters -- polis rides the
      // source block, force rides the root.
      await store.switchSource("model-gateway", {
        polis: "https://polis.test",
        collection: { owner: "acc-x", collection: "beta" },
        force: true,
      });
      assertEquals(seen[0].body, {
        sets: [],
        endpoints: {},
        parking: [],
        source: {
          mode: "model-gateway",
          polis: "https://polis.test",
          collection: { owner: "acc-x", collection: "beta" },
        },
        force: true,
      });
      // An untouched switch keeps both optional fields off the wire.
      await store.switchSource("local");
      assertEquals(seen[1].body, {
        sets: [],
        endpoints: {},
        parking: [],
        source: { mode: "local" },
      });
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test(
  "updateSourceCollection lands the same-mode PUT with force",
  async () => {
    const realFetch = globalThis.fetch;
    const { fetch, seen } = captureFetch(200, switchedConfig);
    globalThis.fetch = fetch;
    try {
      const store = wiredStore();
      // Without a loaded config the mode falls back to local: the
      // same-mode selection update under the local source.
      await store.updateSourceCollection(
        { owner: "acc-x", collection: "alpha" },
        true,
      );
      assertEquals(seen[0].body, {
        sets: [],
        endpoints: {},
        parking: [],
        source: {
          mode: "local",
          collection: { owner: "acc-x", collection: "alpha" },
        },
        force: true,
      });
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test(
  "a dangling 409 parks the stranded list tagged with the send kind",
  async () => {
    const realFetch = globalThis.fetch;
    const { fetch } = captureFetch(409, {
      error: {
        message: "switching to model-gateway drops sets still bound",
        dangling: ["agent-x"],
      },
    });
    globalThis.fetch = fetch;
    try {
      const store = wiredStore();
      await assertRejects(() => store.switchSource("model-gateway"));
      assertEquals(store.pendingDangling, {
        names: ["agent-x"],
        retry: "switch",
      });
      assertEquals(store.error, null);
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test(
  "a dangling 409 on the collection apply parks with retry apply",
  async () => {
    const realFetch = globalThis.fetch;
    const { fetch } = captureFetch(409, {
      error: {
        message:
          "pointing the source at another collection drops sets still bound",
        dangling: ["agent-y"],
      },
    });
    globalThis.fetch = fetch;
    try {
      const store = wiredStore();
      await assertRejects(() =>
        store.updateSourceCollection({ collection: "set-a" }),
      );
      assertEquals(store.pendingDangling, {
        names: ["agent-y"],
        retry: "apply",
      });
      assertEquals(store.error, null);
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test("a dangling 409 on save parks with retry save", async () => {
  const realFetch = globalThis.fetch;
  const { fetch } = captureFetch(409, {
    error: {
      message: "config drops sets still bound",
      dangling: ["agent-z"],
    },
  });
  globalThis.fetch = fetch;
  try {
    const store = wiredStore();
    // save() no-ops without a draft, so seed the committed config
    // and its editable copy directly: the load path is the switch
    // tests' subject, this one targets the save leg.
    store.config = structuredClone(switchedConfig);
    store.draft = structuredClone(switchedConfig);
    await assertRejects(() => store.save());
    assertEquals(store.pendingDangling, {
      names: ["agent-z"],
      retry: "save",
    });
    assertEquals(store.error, null);
  } finally {
    globalThis.fetch = realFetch;
  }
});

Deno.test(
  "refetchSource posts the refresh and adopts the response",
  async () => {
    const realFetch = globalThis.fetch;
    let seen: { method: string; url: string } | null = null;
    globalThis.fetch = ((input: unknown, init?: RequestInit) => {
      seen = {
        method: String(init?.method ?? "GET"),
        url: String(input),
      };
      return Promise.resolve(
        new Response(JSON.stringify(switchedConfig), {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
      );
    }) as typeof fetch;
    try {
      const store = wiredStore();
      await store.refetchSource();
      // The request is the bare refresh: no body, no side writes.
      assertEquals(seen!.method, "POST");
      assertEquals(seen!.url, "http://tagma.test/profiles/refresh");
      // The response folds into both faces, like the switch does.
      assertEquals(store.config, switchedConfig);
      assertEquals(store.draft, switchedConfig);
      assertEquals(store.isRefetching, false);
      assertEquals(store.error, null);
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test("refetchSource failure keeps the committed state", async () => {
  const realFetch = globalThis.fetch;
  globalThis.fetch = ((_input: unknown, _init?: RequestInit) =>
    Promise.resolve(
      new Response(JSON.stringify({ error: { message: "down" } }), {
        status: 502,
        headers: { "content-type": "application/json" },
      }),
    )) as typeof fetch;
  try {
    const store = wiredStore();
    store.config = structuredClone(switchedConfig);
    store.draft = structuredClone(switchedConfig);
    await assertRejects(() => store.refetchSource());
    assertEquals(store.config, switchedConfig);
    assertEquals(store.draft, switchedConfig);
    assertEquals(store.isRefetching, false);
    assertEquals(store.error !== null, true);
  } finally {
    globalThis.fetch = realFetch;
  }
});
