import { assert, assertEquals, assertRejects } from "@std/assert";
import {
  EVERYONE_GROUP_ID,
  UserGatewayError,
  bakedGatewayUrl,
  createProvider,
  deleteProvider,
  isAbsoluteUrl,
  listProviders,
  publishCollection,
  putProviderCredential,
  unpublishCollection,
  updateProfile,
  userFetch,
  createCollection,
  createGroup,
  createProfile,
  createSet,
  deleteCollection,
  deleteGroup,
  deleteProfile,
  deleteProviderCredential,
  deleteSet,
  getCollection,
  getGroup,
  getSet,
  listCollections,
  listProfiles,
  listGroups,
  listSets,
  listPlatformCollections,
  updateCollection,
  updateGroup,
  updateProvider,
  updateSet,
} from "./client.ts";

Deno.test(
  "bakedGatewayUrl reads the runtime config key and defaults to empty",
  () => {
    assertEquals(bakedGatewayUrl(), "");
    (globalThis as { KALLIPAI_CONFIG?: unknown }).KALLIPAI_CONFIG = {
      gatewayAdminUrl: "http://127.0.0.1:7500",
    };
    try {
      assertEquals(bakedGatewayUrl(), "http://127.0.0.1:7500");
    } finally {
      delete (globalThis as { KALLIPAI_CONFIG?: unknown }).KALLIPAI_CONFIG;
    }
  },
);

Deno.test("isAbsoluteUrl is the server's parse predicate", () => {
  assert(isAbsoluteUrl("https://api.example.com/v1"));
  assert(isAbsoluteUrl("http://127.0.0.1:8080/x"));
  assert(!isAbsoluteUrl("/api/v1"));
  assert(!isAbsoluteUrl("api.example.com"));
  assert(!isAbsoluteUrl(""));
});

/** One fake fetch: records the call, answers the canned JSON. */
function stubFetch(
  calls: { url: string; init?: RequestInit }[],
  status = 200,
  body: unknown = {},
): typeof fetch {
  const fake = ((input: unknown, init?: RequestInit) => {
    calls.push({ url: String(input), init });
    const stub = {
      ok: status >= 200 && status < 300,
      status,
      json: () => Promise.resolve(body),
    } as unknown as Response;
    return Promise.resolve(stub);
  }) as unknown as typeof fetch;
  return fake;
}

Deno.test(
  "userFetch rides the session cookie and marks mutations with the CSRF header",
  async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const originalFetch = globalThis.fetch;
    globalThis.fetch = stubFetch(calls);
    try {
      await userFetch("/user/sets", { url: "http://127.0.0.1:7500" });
      await userFetch("/user/sets", {
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
      assertEquals(calls[0]?.init?.credentials, "include");
      assertEquals(calls[1]?.init?.credentials, "include");
      assertEquals(getHeaders.authorization, undefined);
      assertEquals(getHeaders["x-requested-with"], undefined);
      assertEquals(postHeaders["x-requested-with"], "kallipai");
      assertEquals(calls[1]?.init?.body, JSON.stringify({ name: "x" }));
      // content-type rides the body, not the method.
      assertEquals(postHeaders["content-type"], "application/json");
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);

Deno.test(
  "errors map to UserGatewayError with the status and the server message",
  async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const originalFetch = globalThis.fetch;
    globalThis.fetch = stubFetch(calls, 409, {
      error: { message: "collection is already published to this group" },
    });
    try {
      const err = await assertRejects(() =>
        publishCollection("bundle", "everyone", "http://127.0.0.1:7500"),
      );
      assert(err instanceof UserGatewayError);
      assertEquals(err.status, 409);
      assertEquals(
        err.serverMessage,
        "collection is already published to this group",
      );
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);

Deno.test("providers family hits the pool and credential routes", async () => {
  const calls: { url: string; init?: RequestInit }[] = [];
  const originalFetch = globalThis.fetch;
  const row = {
    provider_id: "up1",
    family: "deepseek",
    base_url: null,
    api_key_masked: "sk-…1234",
  };
  globalThis.fetch = stubFetch(calls, 200, { providers: [row] });
  try {
    const list = await listProviders("http://127.0.0.1:7500");
    assertEquals(calls[0]?.url, "http://127.0.0.1:7500/user/providers");
    assertEquals(list, [row]);

    globalThis.fetch = stubFetch(calls, 200, row);
    await createProvider(
      { provider_id: "up1", family: "deepseek" },
      "http://127.0.0.1:7500",
    );
    assertEquals(calls[1]?.init?.method, "POST");

    globalThis.fetch = stubFetch(calls, 200, row);
    await deleteProvider("up1", "http://127.0.0.1:7500");
    assertEquals(calls[2]?.url, "http://127.0.0.1:7500/user/providers/up1");
    assertEquals(calls[2]?.init?.method, "DELETE");

    globalThis.fetch = stubFetch(calls, 200, row);
    await putProviderCredential(
      "up1",
      "https://upstream.example.com",
      "sk-live",
      "http://127.0.0.1:7500",
    );
    assertEquals(
      calls[3]?.url,
      "http://127.0.0.1:7500/user/providers/up1/credential",
    );
    assertEquals(calls[3]?.init?.method, "PUT");
    assertEquals(
      calls[3]?.init?.body,
      JSON.stringify({
        upstream_base_url: "https://upstream.example.com",
        upstream_api_key: "sk-live",
      }),
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test("profiles PUT carries the full-field replacement", async () => {
  const calls: { url: string; init?: RequestInit }[] = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = stubFetch(calls, 200, {});
  try {
    await updateProfile(
      "p1",
      {
        model: "deepseek-chat",
        max_context_window: 4096,
        effort: null,
        modalities: ["text"],
        max_budget: null,
        tpm_limit: null,
        rpm_limit: null,
      },
      "http://127.0.0.1:7500",
    );
    assertEquals(calls[0]?.url, "http://127.0.0.1:7500/user/profiles/p1");
    assertEquals(calls[0]?.init?.method, "PUT");
    const body = JSON.parse(calls[0]?.init?.body as string) as Record<
      string,
      unknown
    >;
    assertEquals(body.model, "deepseek-chat");
    assertEquals(body.max_context_window, 4096);
    assertEquals(body.modalities, ["text"]);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test(
  "publish and unpublish target the reserved audience by id",
  async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const originalFetch = globalThis.fetch;
    globalThis.fetch = stubFetch(calls, 200, { audience: "everyone" });
    try {
      await publishCollection(
        "bundle",
        EVERYONE_GROUP_ID,
        "http://127.0.0.1:7500",
      );
      assertEquals(
        calls[0]?.url,
        "http://127.0.0.1:7500/user/collections/bundle/publications",
      );
      assertEquals(
        calls[0]?.init?.body,
        JSON.stringify({ audience: "everyone" }),
      );
      globalThis.fetch = stubFetch(calls, 200, { deleted: true });
      await unpublishCollection(
        "bundle",
        EVERYONE_GROUP_ID,
        "http://127.0.0.1:7500",
      );
      assertEquals(
        calls[1]?.url,
        "http://127.0.0.1:7500/user/collections/bundle/publications/everyone",
      );
      assertEquals(calls[1]?.init?.method, "DELETE");
      assertEquals(calls[1]?.init?.body, undefined);
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);

Deno.test(
  "every client function speaks only the server's /user route table",
  async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const originalFetch = globalThis.fetch;
    globalThis.fetch = stubFetch(calls, 200, {});
    try {
      await listProviders();
      await createProvider({ provider_id: "p1", family: "deepseek" });
      await updateProvider("p1", { family: "deepseek" });
      await deleteProvider("p1");
      await putProviderCredential("p1", "https://up.example.com", "k");
      await deleteProviderCredential("p1");
      await listProfiles();
      await createProfile({
        profile_id: "pr1",
        provider_id: "p1",
        model: "m",
      });
      await updateProfile("pr1", { model: "m" });
      await deleteProfile("pr1");
      await listSets();
      await getSet("s1");
      await createSet("c1", { name: "s1", description: "" });
      await updateSet("s1", { description: "" });
      await deleteSet("s1");
      await listCollections();
      await getCollection("c1");
      await createCollection({ name: "c1", description: "" });
      await updateCollection("c1", { description: "" });
      await deleteCollection("c1");
      await publishCollection("c1", EVERYONE_GROUP_ID);
      await unpublishCollection("c1", EVERYONE_GROUP_ID);
      await listGroups();
      await getGroup("g1");
      await createGroup({ name: "g1" });
      await updateGroup("g1", { name: "g1" });
      await deleteGroup("g1");
      await listPlatformCollections();
      const seen = calls.map((c) => `${c.init?.method ?? "GET"} ${c.url}`);
      assertEquals(seen, [
        "GET /user/providers",
        "POST /user/providers",
        "PATCH /user/providers/p1",
        "DELETE /user/providers/p1",
        "PUT /user/providers/p1/credential",
        "DELETE /user/providers/p1/credential",
        "GET /user/profiles",
        "POST /user/profiles",
        "PUT /user/profiles/pr1",
        "DELETE /user/profiles/pr1",
        "GET /user/sets",
        "GET /user/sets/s1",
        "POST /user/collections/c1/sets",
        "PATCH /user/sets/s1",
        "DELETE /user/sets/s1",
        "GET /user/collections",
        "GET /user/collections/c1",
        "POST /user/collections",
        "PATCH /user/collections/c1",
        "DELETE /user/collections/c1",
        "POST /user/collections/c1/publications",
        "DELETE /user/collections/c1/publications/everyone",
        "GET /user/groups",
        "GET /user/groups/g1",
        "POST /user/groups",
        "PATCH /user/groups/g1",
        "DELETE /user/groups/g1",
        "GET /user/platform-collections",
      ]);
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);
