import { assertEquals } from "@std/assert";
import { ArcheionClient } from "./client.ts";
import type { TagmaCredentialView, TokenView } from "./types.ts";

// Wire-contract tests for the credential surface (personal tokens + the tagma
// credential). Same shape as the other client tests: stub globalThis.fetch and
// pin the emitted request (path, method, CSRF marker, body) plus the decoded
// response, so a renamed route or a dropped field fails here instead of at
// runtime.

const BASE = "https://archeion.test/v1/archeion";

function withFetch(
  fetchImpl: typeof fetch,
  fn: () => Promise<void>,
): Promise<void> {
  const original = globalThis.fetch;
  globalThis.fetch = fetchImpl;
  return fn().finally(() => {
    globalThis.fetch = original;
  });
}

const TOKEN: TokenView = {
  id: "tok-1",
  name: "laptop-cli",
  masked: "sk-pat-ab***yz",
  perms: ["files:record:read"],
  created_at: "2026-10-01T00:00:00Z",
  expires_at: "2026-12-30T00:00:00Z",
  last_used_at: null,
  revoked_at: null,
};

function capture(
  body: string | null,
  status = 200,
): {
  calls: {
    url: string;
    method: string;
    headers: Record<string, string>;
    body: unknown;
  }[];
  stub: typeof fetch;
} {
  const calls: {
    url: string;
    method: string;
    headers: Record<string, string>;
    body: unknown;
  }[] = [];
  const stub: typeof fetch = (input, init) => {
    calls.push({
      url: typeof input === "string" ? input : input.toString(),
      method: init?.method ?? "GET",
      headers: (init?.headers as Record<string, string>) ?? {},
      body: init?.body ? JSON.parse(init.body as string) : null,
    });
    return Promise.resolve(
      new Response(body, {
        status,
        headers:
          status === 204
            ? { "content-length": "0" }
            : { "content-type": "application/json" },
      }),
    );
  };
  return { calls, stub };
}

Deno.test("listTokens GETs /me/tokens and keeps a revoked row", async () => {
  const revoked = { ...TOKEN, id: "tok-2", revoked_at: "2026-10-05T12:00:00Z" };
  const { calls, stub } = capture(JSON.stringify([TOKEN, revoked]));
  let rows: TokenView[] = [];
  await withFetch(stub, async () => {
    rows = await new ArcheionClient(BASE).listTokens();
  });
  assertEquals(calls[0]!.url, `${BASE}/me/tokens`);
  assertEquals(calls[0]!.method, "GET");
  assertEquals(calls[0]!.headers["X-Requested-With"], undefined);
  assertEquals(rows.length, 2);
  assertEquals(rows[0]!.revoked_at, null);
  assertEquals(rows[1]!.revoked_at, "2026-10-05T12:00:00Z");
});

Deno.test("createToken POSTs the scope and decodes the plaintext", async () => {
  const body = {
    name: "laptop-cli",
    perms: ["files:record:read", "files:record:write"],
    expires_in_days: 30,
  };
  const { calls, stub } = capture(
    JSON.stringify({ token: TOKEN, plaintext: "sk-pat-secret" }),
  );
  let plaintext = "";
  await withFetch(stub, async () => {
    plaintext = (await new ArcheionClient(BASE).createToken(body)).plaintext;
  });
  assertEquals(calls[0]!.url, `${BASE}/me/tokens`);
  assertEquals(calls[0]!.method, "POST");
  assertEquals(calls[0]!.headers["X-Requested-With"], "kallipai");
  assertEquals(calls[0]!.body, body);
  assertEquals(plaintext, "sk-pat-secret");
});

Deno.test("updateToken PATCHes the permission list", async () => {
  const { calls, stub } = capture(JSON.stringify({ ...TOKEN, perms: [] }));
  let perms: readonly string[] = ["unset"];
  await withFetch(stub, async () => {
    perms = (await new ArcheionClient(BASE).updateToken("tok-1", { perms: [] }))
      .perms;
  });
  assertEquals(calls[0]!.url, `${BASE}/me/tokens/tok-1`);
  assertEquals(calls[0]!.method, "PATCH");
  assertEquals(calls[0]!.headers["X-Requested-With"], "kallipai");
  assertEquals(calls[0]!.body, { perms: [] });
  assertEquals(perms.length, 0);
});

Deno.test("revokeToken DELETEs the row and resolves on 204", async () => {
  const { calls, stub } = capture(null, 204);
  await withFetch(stub, async () => {
    await new ArcheionClient(BASE).revokeToken("tok-1");
  });
  assertEquals(calls[0]!.url, `${BASE}/me/tokens/tok-1`);
  assertEquals(calls[0]!.method, "DELETE");
  assertEquals(calls[0]!.headers["X-Requested-With"], "kallipai");
});

Deno.test(
  "getTagmaCredential GETs the tagma row with its history",
  async () => {
    const view: TagmaCredentialView = {
      token: { ...TOKEN, id: "cred-1", name: "dev" },
      events: [
        {
          at: "2026-10-01T00:00:00Z",
          kind: "create",
          actor_user_id: "u-1",
          added: ["files:record:read"],
          removed: [],
        },
      ],
    };
    const { calls, stub } = capture(JSON.stringify(view));
    let got: TagmaCredentialView | null = null;
    await withFetch(stub, async () => {
      got = await new ArcheionClient(BASE).getTagmaCredential("tagma-1");
    });
    assertEquals(calls[0]!.url, `${BASE}/tagmata/tagma-1/credential`);
    assertEquals(calls[0]!.method, "GET");
    assertEquals(got!.events[0]!.kind, "create");
    assertEquals(got!.events[0]!.added, ["files:record:read"]);
  },
);

Deno.test("updateTagmaCredential PATCHes the permission list", async () => {
  const { calls, stub } = capture(JSON.stringify({ token: TOKEN, events: [] }));
  await withFetch(stub, async () => {
    await new ArcheionClient(BASE).updateTagmaCredential("tagma-1", {
      perms: ["files:record:read"],
    });
  });
  assertEquals(calls[0]!.url, `${BASE}/tagmata/tagma-1/credential`);
  assertEquals(calls[0]!.method, "PATCH");
  assertEquals(calls[0]!.body, { perms: ["files:record:read"] });
});

Deno.test("resetTagmaCredential POSTs to the reset route", async () => {
  const { calls, stub } = capture(
    JSON.stringify({ token: TOKEN, plaintext: "sk-internal-new" }),
  );
  let plaintext = "";
  await withFetch(stub, async () => {
    plaintext = (await new ArcheionClient(BASE).resetTagmaCredential("tagma-1"))
      .plaintext;
  });
  assertEquals(calls[0]!.url, `${BASE}/tagmata/tagma-1/credential/reset`);
  assertEquals(calls[0]!.method, "POST");
  assertEquals(calls[0]!.headers["X-Requested-With"], "kallipai");
  assertEquals(plaintext, "sk-internal-new");
});
