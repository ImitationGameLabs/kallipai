// Behavior test for the manage store's mutation judgment: the boolean
// answers whether the action's own mutation landed -- the wrapper's
// explicit true reads as success even when the client wrap folds the
// 200 into void, and run's undefined failure marker reads as false.
// The wire is faked at fetch (200 for the landed branch, 404 for the
// refused branch), so the real client+store chain runs end to end.
//
// The store module is rune-bearing, but deno test runs it uncompiled,
// so a passthrough $state shim lets it run with plain fields (the
// user-family store_test pattern).
import type { AdminProfileRow } from "./client.ts";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const mod = await import("./store.svelte.ts");
const store: typeof mod.gatewayStore = mod.gatewayStore;

/** A catalog row as the wire spells it: the fields a member PUT carries,
 * and the set that holds it. */
function row(
  collection: string,
  id: string,
  overrides: Partial<AdminProfileRow> = {},
): AdminProfileRow {
  return {
    profile_id: id,
    collection_name: collection,
    provider_id: `up-${id}`,
    owner: "admin",
    family: "deepseek",
    model: `${id}-model`,
    max_context_window: 128_000,
    effort: null,
    modalities: ["text"],
    set: null,
    position: 0,
    max_budget: null,
    tpm_limit: null,
    rpm_limit: null,
    ...overrides,
  };
}

Deno.test(
  "mutation answers true when the wire lands, false when refused",
  async () => {
    const realFetch = globalThis.fetch;
    const respond = (status: number) => {
      globalThis.fetch = () =>
        Promise.resolve(
          new Response(JSON.stringify({}), {
            status,
            headers: { "content-type": "application/json" },
          }),
        );
    };
    try {
      // 200 folds to void in these wraps; the explicit return inside
      // run's callback must still read as true.
      respond(200);
      assertEquals(await store.saveCredential("p1", "https://x", "sk"), true);
      assertEquals(await store.removeCredential("p1"), true);
      assertEquals(await store.addSet("baseline", "s1", "d"), true);
      assertEquals(await store.setCollectionDefault("baseline", "s1"), true);
      // 404 raises the client's admin error, which run folds to the
      // undefined marker; the action answers false.
      respond(404);
      assertEquals(await store.saveCredential("p1", "https://x", "sk"), false);
      assertEquals(await store.removeCredential("p1"), false);
      assertEquals(await store.addSet("baseline", "s1", "d"), false);
      assertEquals(await store.setCollectionDefault("baseline", "s1"), false);
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test("a set save writes and does not read", async () => {
  const realFetch = globalThis.fetch;
  const seen: string[] = [];
  // Two writes and no read at all: the read side belongs to the save
  // sequence, so a store write that refreshed behind its own back would
  // double the reads the sequence already paid for.
  globalThis.fetch = ((input: unknown, init?: { method?: string }) => {
    seen.push(`${init?.method ?? "GET"} ${String(input)}`);
    return Promise.resolve(
      new Response(
        JSON.stringify({ name: "s1", description: "d", members: [] }),
        { status: 200, headers: { "content-type": "application/json" } },
      ),
    );
  }) as unknown as typeof fetch;
  try {
    assertEquals(await store.patchDescription("baseline", "s1", "d"), true);
    assertEquals(await store.reorderSet("baseline", "s1", ["a", "b"]), true);
    assertEquals(seen.length, 2);
    assertEquals(seen[0]?.startsWith("PATCH "), true);
    assertEquals(
      seen[0]?.endsWith("/admin/collections/baseline/sets/s1"),
      true,
    );
    assertEquals(seen[1]?.startsWith("PUT "), true);
    assertEquals(
      seen[1]?.endsWith("/admin/collections/baseline/sets/s1/order"),
      true,
    );
  } finally {
    globalThis.fetch = realFetch;
  }
});

Deno.test(
  "collection mutations surface failures on the store's error state and refreshAll populates the reach state",
  async () => {
    const realFetch = globalThis.fetch;
    const respond = (status: number) => {
      globalThis.fetch = () =>
        Promise.resolve(
          new Response(JSON.stringify({}), {
            status,
            headers: { "content-type": "application/json" },
          }),
        );
    };
    try {
      // The reach state fills from the two new reads in refreshAll; the
      // rest of the family answers its wrapped empties.
      globalThis.fetch = ((input: unknown) => {
        const url = String(input);
        const payloads: [string, unknown][] = [
          ["/admin/providers", { providers: [] }],
          ["/admin/profiles?unassigned=true", []],
          ["/admin/collections/alpha/sets", { sets: [] }],
          [
            "/admin/collections",
            {
              collections: [
                {
                  name: "alpha",
                  description: "d",
                  sets: ["s1"],
                  publications: ["everyone"],
                },
              ],
            },
          ],
          [
            "/admin/groups",
            {
              groups: [{ group_id: "everyone", name: "Everyone", members: [] }],
            },
          ],
        ];
        const payload =
          payloads.find(([suffix]) => url.endsWith(suffix))?.[1] ?? {};
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve(payload),
        } as unknown as Response);
      }) as unknown as typeof fetch;
      await store.refreshAll();
      assertEquals(store.collections?.[0]?.name, "alpha");
      assertEquals(store.groups?.[0]?.group_id, "everyone");
      assertEquals(store.hasLoaded, true);
      // The void collection actions answer through the store's error
      // state (the dialogs read it to stay open): a landed wire keeps
      // it clear, a refused one records the reason.
      respond(200);
      assertEquals(
        await store.saveCollection("alpha", { description: "d2" }),
        true,
      );
      assertEquals(store.error, null);
      await store.publishToGroup("alpha", "everyone");
      assertEquals(store.error, null);
      await store.unpublishFromGroup("alpha", "everyone");
      assertEquals(store.error, null);
      await store.removeCollection("alpha");
      assertEquals(store.error, null);
      assertEquals(
        await store.addCollection({ name: "b", description: "" }),
        true,
      );
      respond(404);
      assertEquals(
        await store.addCollection({ name: "b", description: "" }),
        false,
      );
      assertEquals(
        await store.saveCollection("alpha", { description: "d2" }),
        false,
      );
      assertEquals(typeof store.error, "string");
      await store.publishToGroup("alpha", "everyone");
      assertEquals(typeof store.error, "string");
      await store.unpublishFromGroup("alpha", "everyone");
      assertEquals(typeof store.error, "string");
      await store.removeCollection("alpha");
      assertEquals(typeof store.error, "string");
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test(
  "group mutations answer like the collection family: create and save answer booleans, removals ride the error state",
  async () => {
    const realFetch = globalThis.fetch;
    const respond = (status: number) => {
      globalThis.fetch = () =>
        Promise.resolve(
          new Response(JSON.stringify({}), {
            status,
            headers: { "content-type": "application/json" },
          }),
        );
    };
    try {
      respond(200);
      assertEquals(await store.addGroup({ name: "g1" }), true);
      assertEquals(await store.saveGroup("g1", { name: "g2" }), true);
      await store.addMember("g1", "acc-1");
      assertEquals(store.error, null);
      await store.removeMember("g1", "acc-1");
      assertEquals(store.error, null);
      await store.removeGroup("g1");
      assertEquals(store.error, null);
      respond(404);
      assertEquals(await store.addGroup({ name: "g1" }), false);
      assertEquals(await store.saveGroup("g1", { name: "g2" }), false);
      await store.addMember("g1", "acc-1");
      assertEquals(typeof store.error, "string");
      await store.removeMember("g1", "acc-1");
      assertEquals(typeof store.error, "string");
      await store.removeGroup("g1");
      assertEquals(typeof store.error, "string");
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test(
  "unassigned mutations answer like the group family: create and save answer booleans, removal rides the error state",
  async () => {
    const realFetch = globalThis.fetch;
    const calls: { method: string; url: string; body?: string }[] = [];
    const respond = (status: number) => {
      globalThis.fetch = ((input: unknown, init?: RequestInit) => {
        calls.push({
          method: init?.method ?? "GET",
          url: String(input),
          body: typeof init?.body === "string" ? init.body : undefined,
        });
        return Promise.resolve(
          new Response(JSON.stringify({}), {
            status,
            headers: { "content-type": "application/json" },
          }),
        );
      }) as typeof fetch;
    };
    const fields = {
      model: "gpt",
      max_context_window: null,
      effort: null,
      modalities: null,
    };
    try {
      respond(200);
      assertEquals(
        await store.addUnassigned("alpha", {
          profile_id: "p1",
          provider_id: "up1",
          model: "gpt",
        }),
        true,
      );
      assertEquals(
        await store.saveUnassigned(row("alpha", "p1"), fields),
        true,
      );
      // The PUT replaces the whole row, so the set rides the body too; this
      // row is unassigned, so it is null.
      assertEquals(
        JSON.parse(
          calls.find(
            (call) =>
              call.method === "PUT" &&
              call.url.endsWith("/admin/collections/alpha/profiles/p1"),
          )!.body ?? "{}",
        ),
        { ...fields, set: null },
      );
      await store.removeUnassigned("alpha", "p1");
      assertEquals(store.error, null);
      respond(404);
      assertEquals(
        await store.addUnassigned("alpha", {
          profile_id: "p1",
          provider_id: "up1",
          model: "gpt",
        }),
        false,
      );
      assertEquals(
        await store.saveUnassigned(row("alpha", "p1"), fields),
        false,
      );
      await store.removeUnassigned("alpha", "p1");
      assertEquals(typeof store.error, "string");
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test("saveUnassigned carries the set the row holds", async () => {
  const realFetch = globalThis.fetch;
  const bodies: string[] = [];
  globalThis.fetch = ((_input: unknown, init?: RequestInit) => {
    if ((init?.method ?? "GET") === "PUT" && typeof init?.body === "string") {
      bodies.push(init.body);
    }
    return Promise.resolve(
      new Response(JSON.stringify({}), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
    );
  }) as typeof fetch;
  try {
    // The page opens this dialog on an unassigned row only, whose set is
    // null; the test pins the rule rather than a reachable row: the body
    // states the set the row holds, never a constant.
    assertEquals(
      await store.saveUnassigned(row("alpha", "p1", { set: "s1" }), {
        model: "gpt",
        max_context_window: null,
        effort: null,
        modalities: null,
      }),
      true,
    );
    assertEquals(JSON.parse(bodies[0] ?? "{}").set, "s1");
  } finally {
    globalThis.fetch = realFetch;
  }
});

Deno.test(
  "addUnassignedToSet PUTs the row with its destination set and no-ops on a row already inside",
  async () => {
    const realFetch = globalThis.fetch;
    const calls: { method: string; url: string; body?: string }[] = [];
    const json = (v: unknown, status = 200) =>
      new Response(JSON.stringify(v), {
        status,
        headers: { "content-type": "application/json" },
      });
    globalThis.fetch = ((input: unknown, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? "GET";
      calls.push({
        method,
        url,
        body: typeof init?.body === "string" ? init.body : undefined,
      });
      if (url.endsWith("/admin/providers")) {
        return Promise.resolve(json({ providers: [] }));
      }
      if (url.includes("/admin/profiles")) return Promise.resolve(json([]));
      if (url.endsWith("/admin/collections")) {
        return Promise.resolve(json({ collections: [] }));
      }
      if (url.endsWith("/admin/groups")) {
        return Promise.resolve(json({ groups: [] }));
      }
      return Promise.resolve(json({}));
    }) as typeof fetch;
    try {
      assertEquals(
        await store.addUnassignedToSet(row("alpha", "b"), "s1"),
        true,
      );
      const puts = calls.filter((c) => c.method === "PUT");
      assertEquals(puts.length, 1);
      assertEquals(
        puts[0]!.url.endsWith("/admin/collections/alpha/profiles/b"),
        true,
      );
      // One PUT of the row's own content, naming the destination set: the
      // server appends the row at that set's last place.
      assertEquals(JSON.parse(puts[0]!.body ?? "{}"), {
        model: "b-model",
        max_context_window: 128_000,
        effort: null,
        modalities: ["text"],
        set: "s1",
      });
      // A row the set already carries lands as a no-op: no request at all.
      calls.length = 0;
      assertEquals(
        await store.addUnassignedToSet(row("alpha", "a", { set: "s1" }), "s1"),
        true,
      );
      assertEquals(calls.length, 0);
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);
Deno.test(
  "addUnassignedToSet answers false when its PUT is refused",
  async () => {
    const realFetch = globalThis.fetch;
    const calls: { method: string; url: string }[] = [];
    globalThis.fetch = ((input: unknown, init?: RequestInit) => {
      const url = String(input);
      calls.push({ method: init?.method ?? "GET", url });
      return Promise.resolve(
        new Response(JSON.stringify({}), {
          status: url.endsWith("/admin/collections/alpha/profiles/b")
            ? 404
            : 200,
          headers: { "content-type": "application/json" },
        }),
      );
    }) as typeof fetch;
    try {
      assertEquals(
        await store.addUnassignedToSet(row("alpha", "b"), "s1"),
        false,
      );
      assertEquals(calls.filter((c) => c.method === "PUT").length, 1);
      assertEquals(typeof store.error, "string");
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);
Deno.test(
  "updateMember PUTs the row as a served member and answers whether it landed",
  async () => {
    const realFetch = globalThis.fetch;
    const calls: { method: string; url: string; body: unknown }[] = [];
    let status = 200;
    globalThis.fetch = ((input: unknown, init?: RequestInit) => {
      calls.push({
        method: init?.method ?? "GET",
        url: String(input),
        body: typeof init?.body === "string" ? JSON.parse(init.body) : null,
      });
      return Promise.resolve(
        new Response(JSON.stringify({}), {
          status,
          headers: { "content-type": "application/json" },
        }),
      );
    }) as typeof fetch;
    const body = {
      model: "deepseek-next",
      max_context_window: 128_000,
      effort: "high",
      modalities: ["text", "image"],
      set: "s1",
    };
    try {
      assertEquals(await store.updateMember("alpha", "p1", body), true);
      assertEquals(calls.length, 1);
      assertEquals(calls[0]!.method, "PUT");
      assertEquals(
        calls[0]!.url.endsWith("/admin/collections/alpha/profiles/p1"),
        true,
      );
      // The body names the set that keeps the row served: a member PUT
      // never carries the row back into the unassigned pool.
      assertEquals(calls[0]!.body, body);

      status = 404;
      assertEquals(await store.updateMember("alpha", "p1", body), false);
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test(
  "a refused set description answers false and records the reason",
  async () => {
    const realFetch = globalThis.fetch;
    globalThis.fetch = () =>
      Promise.resolve(new Response(JSON.stringify({}), { status: 500 }));
    try {
      assertEquals(await store.patchDescription("baseline", "s1", "d"), false);
      assertEquals(store.error !== null, true);
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);
