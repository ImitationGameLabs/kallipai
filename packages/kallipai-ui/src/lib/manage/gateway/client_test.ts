import { assert, assertEquals, assertRejects } from "@std/assert";
import {
  addGroupMember,
  AdminError,
  adminFetch,
  bakedAdminUrl,
  createCollection,
  createGroup,
  deleteCollection,
  deleteGroup,
  getCollection,
  isConnected,
  listCollections,
  listCollectionSets,
  listGroups,
  listParking,
  listProviders,
  publishCollection,
  putProviderCredential,
  removeGroupMember,
  searchAccounts,
  setCollectionDefault,
  unpublishCollection,
  updateCollection,
  updateGroup,
} from "./client.ts";

Deno.test(
  "bakedAdminUrl reads the runtime config key and defaults to empty",
  () => {
    assertEquals(bakedAdminUrl(), "");
    (globalThis as { KALLIPAI_CONFIG?: unknown }).KALLIPAI_CONFIG = {
      gatewayAdminUrl: "http://127.0.0.1:7500",
    };
    try {
      assertEquals(bakedAdminUrl(), "http://127.0.0.1:7500");
    } finally {
      delete (globalThis as { KALLIPAI_CONFIG?: unknown }).KALLIPAI_CONFIG;
    }
  },
);

Deno.test("isConnected reads only the baked url", () => {
  assert(!isConnected(""));
  assert(isConnected("http://gw"));
});

Deno.test(
  "adminFetch rides the session cookie and marks mutations with the CSRF header",
  async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const originalFetch = globalThis.fetch;
    const fakeFetch = ((input: unknown, init?: RequestInit) => {
      calls.push({ url: String(input), init });
      // The client only reads ok/status/json off the response; a minimal
      // stand-in sidesteps Response-constructor typing differences.
      const stub = {
        ok: true,
        status: 200,
        json: () => Promise.resolve({ sets: [] }),
      } as unknown as Response;
      return Promise.resolve(stub);
    }) as unknown as typeof fetch;
    globalThis.fetch = fakeFetch;
    try {
      await adminFetch("/admin/collections/main/sets", {
        url: "http://127.0.0.1:7500",
      });
      await adminFetch("/admin/collections/main/sets", {
        method: "POST",
        body: { name: "x" },
        url: "http://127.0.0.1:7500",
      });
      const getHeaders = Object.fromEntries(
        new Headers(calls[0]?.init?.headers),
      );
      const postHeaders = Object.fromEntries(
        new Headers(calls[1]?.init?.headers),
      );
      // Every request carries the session cookie and no credential header:
      // the archeion session is the only auth the face accepts.
      assertEquals(calls[0]?.init?.credentials, "include");
      assertEquals(calls[1]?.init?.credentials, "include");
      assertEquals(getHeaders.authorization, undefined);
      assertEquals(postHeaders.authorization, undefined);
      // Mutations carry the CSRF marker; reads do not (the guard only
      // checks state-changing requests).
      assertEquals(getHeaders["x-requested-with"], undefined);
      assertEquals(postHeaders["x-requested-with"], "kallipai");
      assertEquals(calls[1]?.init?.body, JSON.stringify({ name: "x" }));
      // content-type rides the body, not the method: a bodied POST
      // carries it, a body-less DELETE does not.
      assertEquals(postHeaders["content-type"], "application/json");
      await adminFetch("/admin/sets/alpha", {
        method: "DELETE",
        url: "http://127.0.0.1:7500",
      });
      const deleteHeaders = Object.fromEntries(
        new Headers(calls[2]?.init?.headers),
      );
      assertEquals(deleteHeaders["x-requested-with"], "kallipai");
      assertEquals(deleteHeaders["content-type"], undefined);
      globalThis.fetch = (() => {
        const stub = {
          ok: false,
          status: 409,
          json: () =>
            Promise.resolve({ error: { message: "set already exists" } }),
        } as unknown as Response;
        return Promise.resolve(stub);
      }) as unknown as typeof fetch;
      const err = await assertRejects(
        () =>
          adminFetch("/admin/collections/main/sets", {
            method: "POST",
            body: { name: "x" },
            url: "http://127.0.0.1:7500",
          }),
        AdminError,
      );
      assertEquals(err.status, 409);
      assertEquals(err.serverMessage, "set already exists");
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);

Deno.test("provider wraps speak the server's wire shapes", async () => {
  const calls: { url: string; init?: RequestInit }[] = [];
  const originalFetch = globalThis.fetch;
  // list: the server answers ProviderViews {providers: [...]} with the
  // mask, not a bare id array.
  globalThis.fetch = ((input: unknown, init?: RequestInit) => {
    calls.push({ url: String(input), init });
    const stub = {
      ok: true,
      status: 200,
      json: () =>
        Promise.resolve({
          providers: [
            {
              owner: "system",
              provider_id: "p1",
              family: "deepseek",
              base_url: "https://api.upstream.test",
              api_key_masked: "sk-***-123",
            },
          ],
        }),
    } as unknown as Response;
    return Promise.resolve(stub);
  }) as unknown as typeof fetch;
  try {
    const rows = await listProviders();
    assertEquals(rows.length, 1);
    assertEquals(rows[0]?.provider_id, "p1");
    assertEquals(rows[0]?.api_key_masked, "sk-***-123");
    // put: the body names upstream_api_key (not api_key) and carries the
    // absolute base URL the form collected.
    await putProviderCredential("p1", "https://api.upstream.test", "sk-plain");
    const body = JSON.parse(String(calls[1]?.init?.body));
    assertEquals(body.upstream_api_key, "sk-plain");
    assertEquals(body.upstream_base_url, "https://api.upstream.test");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test("sets and parking reads speak the server's shapes", async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = ((input: unknown) => {
    const url = String(input);
    let payload: unknown;
    if (url.endsWith("/admin/collections/baseline/sets")) {
      // AdminSetList wraps the nested list.
      payload = {
        sets: [{ name: "alpha", description: "d", profiles: [] }],
      };
    } else if (url.endsWith("/admin/parking")) {
      // list_parked answers a bare array -- the one unwrap-less GET
      // in the family (no {profiles: [...]} wrapper exists).
      payload = [
        { profile_id: "p1", family: "openai", model: "m", parked: true },
      ];
    }
    const stub = {
      ok: true,
      status: 200,
      json: () => Promise.resolve(payload),
    } as unknown as Response;
    return Promise.resolve(stub);
  }) as unknown as typeof fetch;
  try {
    const sets = await listCollectionSets("baseline");
    assertEquals(sets[0]?.name, "alpha");
    const parked = await listParking();
    assertEquals(parked[0]?.profile_id, "p1");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test(
  "listParking reads the one parking list the face serves",
  async () => {
    const calls: string[] = [];
    const originalFetch = globalThis.fetch;
    globalThis.fetch = ((input: unknown) => {
      calls.push(String(input));
      const stub = {
        ok: true,
        status: 200,
        json: () => Promise.resolve([]),
      } as unknown as Response;
      return Promise.resolve(stub);
    }) as unknown as typeof fetch;
    try {
      await listParking("http://127.0.0.1:7500");
      assertEquals(calls, ["http://127.0.0.1:7500/admin/parking"]);
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);

Deno.test(
  "collection and group wraps speak the server's wire shapes",
  async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const originalFetch = globalThis.fetch;
    globalThis.fetch = ((input: unknown, init?: RequestInit) => {
      calls.push({ url: String(input), init });
      const url = String(input);
      let payload: unknown;
      if (url.endsWith("/admin/groups")) {
        // UserGroupViews wraps the platform audiences.
        payload = {
          groups: [{ group_id: "everyone", name: "Everyone", members: [] }],
        };
      } else if (url.endsWith("/admin/collections")) {
        // list: AdminCollectionViews wraps the admin views, which
        // carry the live publications the user face's shape omits.
        payload = {
          collections: [
            {
              name: "alpha",
              description: "d",
              sets: ["s1"],
              publications: ["everyone"],
            },
          ],
        };
      } else if (
        url.endsWith("/publications") ||
        url.includes("/publications/")
      ) {
        payload = { ok: true };
      } else {
        // get/update/delete answer the single admin view; delete's
        // Deleted shape parses the same way through our wraps.
        payload = {
          name: "alpha",
          description: "d",
          sets: ["s1"],
          publications: [],
        };
      }
      const stub = {
        ok: true,
        status: 200,
        json: () => Promise.resolve(payload),
      } as unknown as Response;
      return Promise.resolve(stub);
    }) as unknown as typeof fetch;
    try {
      const rows = await listCollections();
      assertEquals(rows.length, 1);
      assertEquals(rows[0]?.name, "alpha");
      assertEquals(rows[0]?.publications, ["everyone"]);
      const got = await getCollection("alpha");
      assertEquals(got.sets, ["s1"]);
      assert(calls[1]?.url.endsWith("/admin/collections/alpha"));
      await createCollection({ name: "beta", description: "x" });
      assertEquals(calls[2]?.init?.method, "POST");
      assertEquals(JSON.parse(String(calls[2]?.init?.body)), {
        name: "beta",
        description: "x",
      });
      await updateCollection("alpha", { description: "y" });
      assertEquals(calls[3]?.init?.method, "PATCH");
      assertEquals(JSON.parse(String(calls[3]?.init?.body)), {
        description: "y",
      });
      await deleteCollection("beta");
      assertEquals(calls[4]?.init?.method, "DELETE");
      await publishCollection("alpha", "everyone");
      assert(calls[5]?.url.endsWith("/admin/collections/alpha/publications"));
      assertEquals(JSON.parse(String(calls[5]?.init?.body)), {
        audience: "everyone",
      });
      await unpublishCollection("alpha", "g2");
      assert(
        calls[6]?.url.endsWith("/admin/collections/alpha/publications/g2"),
      );
      assertEquals(calls[6]?.init?.method, "DELETE");
      await setCollectionDefault("alpha", "s1");
      assert(calls[7]?.url.endsWith("/admin/collections/alpha/default"));
      assertEquals(calls[7]?.init?.method, "PATCH");
      assertEquals(JSON.parse(String(calls[7]?.init?.body)), {
        set_name: "s1",
      });
      const groups = await listGroups();
      assertEquals(groups[0]?.group_id, "everyone");
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);

Deno.test(
  "group wraps and the account search speak the server's wire shapes",
  async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const originalFetch = globalThis.fetch;
    globalThis.fetch = ((input: unknown, init?: RequestInit) => {
      calls.push({ url: String(input), init });
      const url = String(input);
      let payload: unknown;
      if (url.endsWith("/admin/accounts/search")) {
        // AccountSearchView: the four identity fields, never the email.
        payload = {
          users: [
            {
              account_id: "acc-1",
              username: "ada",
              display_name: "Ada",
              disabled: false,
            },
            {
              account_id: "acc-2",
              username: "bob",
              // The server answers null for an unset display name.
              display_name: null,
              disabled: true,
            },
          ],
        };
      } else if (url.endsWith("/members/acc-9")) {
        payload = { group_id: "g1", name: "n", members: [] };
      } else if (url.endsWith("/members")) {
        payload = { group_id: "g1", name: "n", members: ["acc-1"] };
      } else if (url.endsWith("/admin/groups")) {
        if (init?.method === "POST") {
          // create answers the single view.
          payload = { group_id: "g1", name: "g", members: [] };
        } else {
          // list: UserGroupViews.
          payload = {
            groups: [{ group_id: "g1", name: "n", members: ["acc-1"] }],
          };
        }
      } else {
        payload = { group_id: "g1", name: "n2", members: [] };
      }
      const stub = {
        ok: true,
        status: 200,
        json: () => Promise.resolve(payload),
      } as unknown as Response;
      return Promise.resolve(stub);
    }) as unknown as typeof fetch;
    try {
      const groups = await listGroups();
      assertEquals(groups[0]?.members, ["acc-1"]);
      const made = await createGroup({ name: "g" });
      assertEquals(made.group_id, "g1");
      assertEquals(calls[1]?.init?.method, "POST");
      const saved = await updateGroup("g1", { name: "n2" });
      assertEquals(saved.name, "n2");
      assertEquals(calls[2]?.init?.method, "PATCH");
      const added = await addGroupMember("g1", "acc-1");
      assertEquals(added.members, ["acc-1"]);
      assertEquals(JSON.parse(String(calls[3]?.init?.body)), {
        member_account: "acc-1",
      });
      await removeGroupMember("g1", "acc-9");
      assert(calls[4]?.url.endsWith("/admin/groups/g1/members/acc-9"));
      await deleteGroup("g1");
      assertEquals(calls[5]?.init?.method, "DELETE");
      const users = await searchAccounts("ad");
      assertEquals(users[0]?.account_id, "acc-1");
      assertEquals(users[1]?.username, "bob");
      assertEquals(users[1]?.display_name, null);
      assertEquals(users[1]?.disabled, true);
      assertEquals(Object.keys(users[0] ?? {}).sort(), [
        "account_id",
        "disabled",
        "display_name",
        "username",
      ]);
      assertEquals(JSON.parse(String(calls[6]?.init?.body)), { query: "ad" });
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);
