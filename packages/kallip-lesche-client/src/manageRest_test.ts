// Tests pin the manage client's pass-through contract: any status
// resolves with the raw body (never throws for non-2xx), proxy text
// errors land as string bodies, and the header shape matches what the
// lesche csrf_guard expects.

import { assertEquals } from "@std/assert";
import { ManageRestClient } from "./manageRest.ts";

/** Swap global fetch for the duration of `run`; restores it after. */
async function withFetch<T>(
  stub: (input: string | URL, init?: RequestInit) => Promise<Response>,
  run: () => Promise<T>,
): Promise<T> {
  const original = globalThis.fetch;
  globalThis.fetch = stub as typeof fetch;
  try {
    return await run();
  } finally {
    globalThis.fetch = original;
  }
}

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

Deno.test("a 2xx JSON body passes through untouched", async () => {
  const client = new ManageRestClient("https://lesche.example");
  await withFetch(
    () => Promise.resolve(jsonResponse(200, { agents: [] })),
    async () => {
      const result = await client.manage("t-a", "GET", "/agents");
      assertEquals(result, { status: 200, body: { agents: [] } });
    },
  );
});

Deno.test(
  "a plain-text 403 resolves as a string body instead of throwing",
  async () => {
    const client = new ManageRestClient("https://lesche.example");
    await withFetch(
      () =>
        Promise.resolve(
          new Response("not your tagma", {
            status: 403,
            headers: { "content-type": "text/plain" },
          }),
        ),
      async () => {
        const result = await client.manage("t-a", "GET", "/budget");
        // The string body is the shape parseError's string branch
        // folds into a KallipError.
        assertEquals(result, { status: 403, body: "not your tagma" });
      },
    );
  },
);

Deno.test("an empty body resolves as null", async () => {
  const client = new ManageRestClient("https://lesche.example");
  await withFetch(
    () => Promise.resolve(new Response(null, { status: 204 })),
    async () => {
      const result = await client.manage("t-a", "DELETE", "/agents/a-1");
      assertEquals(result, { status: 204, body: null });
    },
  );
});

Deno.test("a non-JSON body falls back to the raw text", async () => {
  const client = new ManageRestClient("https://lesche.example");
  await withFetch(
    () =>
      Promise.resolve(
        new Response("<html>bad gateway</html>", { status: 502 }),
      ),
    async () => {
      const result = await client.manage("t-a", "GET", "/budget");
      assertEquals(result, { status: 502, body: "<html>bad gateway</html>" });
    },
  );
});

Deno.test("the CSRF marker rides only on mutating requests", async () => {
  const client = new ManageRestClient("https://lesche.example");
  const seen: RequestInit[] = [];
  await withFetch(
    (_input, init) => {
      seen.push(init ?? {});
      return Promise.resolve(jsonResponse(200, {}));
    },
    async () => {
      await client.manage("t-a", "GET", "/budget");
      await client.manage("t-a", "POST", "/budget", { daily: 5 });
    },
  );
  const getHeaders = seen[0]!.headers as Record<string, string>;
  const postHeaders = seen[1]!.headers as Record<string, string>;
  assertEquals(getHeaders["X-Requested-With"], undefined);
  assertEquals(postHeaders["X-Requested-With"], "kallip");
});

Deno.test("content-type rides only when a JSON body is present", async () => {
  const client = new ManageRestClient("https://lesche.example");
  const seen: RequestInit[] = [];
  await withFetch(
    (_input, init) => {
      seen.push(init ?? {});
      return Promise.resolve(jsonResponse(200, {}));
    },
    async () => {
      await client.manage("t-a", "POST", "/budget");
      await client.manage("t-a", "POST", "/budget", { daily: 5 });
    },
  );
  const bare = seen[0]!.headers as Record<string, string>;
  const withBody = seen[1]!.headers as Record<string, string>;
  assertEquals(bare["content-type"], undefined);
  assertEquals(withBody["content-type"], "application/json");
});

Deno.test("the agent segment is URL-encoded in the request path", async () => {
  const client = new ManageRestClient("https://lesche.example/v1/lesche");
  let url = "";
  await withFetch(
    (input) => {
      url = String(input);
      return Promise.resolve(jsonResponse(200, {}));
    },
    async () => {
      await client.manage("ag/ent 1", "GET", "/budget");
    },
  );
  assertEquals(
    url,
    "https://lesche.example/v1/lesche/tagmata/ag%2Fent%201/manage/budget",
  );
});
