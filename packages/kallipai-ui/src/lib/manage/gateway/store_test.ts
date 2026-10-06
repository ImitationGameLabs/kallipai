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
declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const mod = await import("./store.svelte.ts");
const store: typeof mod.gatewayStore = mod.gatewayStore;

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
          ["/admin/parking", []],
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
  "parked-draft mutations answer like the group family: create and save answer booleans, removal rides the error state",
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
      assertEquals(
        await store.addParked({
          profile_id: "p1",
          provider_id: "up1",
          model: "gpt",
        }),
        true,
      );
      assertEquals(
        await store.saveParked("p1", { model: "gpt", parked: true }),
        true,
      );
      await store.removeParked("p1");
      assertEquals(store.error, null);
      respond(404);
      assertEquals(
        await store.addParked({
          profile_id: "p1",
          provider_id: "up1",
          model: "gpt",
        }),
        false,
      );
      assertEquals(
        await store.saveParked("p1", { model: "gpt", parked: true }),
        false,
      );
      await store.removeParked("p1");
      assertEquals(typeof store.error, "string");
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test(
  "addParkedToSet puts the appended member list and no-ops on an id already inside",
  async () => {
    const realFetch = globalThis.fetch;
    const calls: { method: string; url: string; body?: string }[] = [];
    const row = (ids: string[]) => ({
      name: "s1",
      description: "",
      owner: "admin",
      profiles: ids.map((id) => ({ profile_id: id })),
    });
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
      if (url.endsWith("/admin/sets/s1")) {
        return Promise.resolve(
          json(method === "GET" ? row(["a"]) : row(["a", "b"])),
        );
      }
      if (url.endsWith("/admin/parking")) return Promise.resolve(json([]));
      if (url.endsWith("/admin/providers")) {
        return Promise.resolve(json({ providers: [] }));
      }
      if (url.endsWith("/admin/collections")) {
        return Promise.resolve(json({ collections: [] }));
      }
      if (url.endsWith("/admin/groups")) {
        return Promise.resolve(json({ groups: [] }));
      }
      return Promise.resolve(json({}));
    }) as typeof fetch;
    try {
      assertEquals(await store.addParkedToSet("b", "s1"), true);
      const updates = calls.filter((c) => c.method === "PATCH");
      assertEquals(updates.length, 1);
      assertEquals(JSON.parse(updates[0].body ?? "{}"), {
        members: ["a", "b"],
      });
      // An id already inside the set lands as a no-op drop: no second update.
      calls.length = 0;
      assertEquals(await store.addParkedToSet("a", "s1"), true);
      assertEquals(calls.filter((c) => c.method === "PATCH").length, 0);
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);

Deno.test(
  "addParkedToSet skips the update when the set read fails",
  async () => {
    const realFetch = globalThis.fetch;
    const calls: { method: string; url: string }[] = [];
    globalThis.fetch = ((_input: unknown, init?: RequestInit) => {
      const url = String(_input);
      calls.push({ method: init?.method ?? "GET", url });
      return Promise.resolve(
        new Response(JSON.stringify({}), {
          status: url.endsWith("/admin/sets/s1") ? 404 : 200,
          headers: { "content-type": "application/json" },
        }),
      );
    }) as typeof fetch;
    try {
      assertEquals(await store.addParkedToSet("b", "s1"), false);
      assertEquals(calls.filter((c) => c.method === "PATCH").length, 0);
    } finally {
      globalThis.fetch = realFetch;
    }
  },
);
