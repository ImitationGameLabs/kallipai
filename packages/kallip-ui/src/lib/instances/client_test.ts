// Locks the tagma->instance slug convention: the one-click spawn names the
// instance and the devices-list join looks it up, so a silent change to the
// prefix shape would break the join with no signal. These cases pin the
// mapping (and its deliberate first-8-chars collision semantics).
import { InstancesClient, instanceSlugFor } from "./client.ts";
import { assertEquals } from "@std/assert";

Deno.test("instanceSlugFor derives the one-click spawn slug", () => {
  assertEquals(instanceSlugFor("abcdefgh-1234-xyz"), "tagma-abcdefgh");
});

Deno.test(
  "ids differing only past the first 8 chars map to the same slug",
  () => {
    // Deliberate: the convention trades a rare collision for a short, readable
    // slug -- the join treats such identities as one device.
    assertEquals(
      instanceSlugFor("abcdefgh-AAAA"),
      instanceSlugFor("abcdefgh-BBBB"),
    );
  },
);

// The credential shape is locked here too: every exchange is credentialed
// (the session cookie channel) and non-GETs carry the CSRF marker the
// instances guard demands -- a silent regression flips the page back to
// the 401 banner it was built to retire.
Deno.test(
  "client sends credentialed fetches with the CSRF marker",
  async () => {
    const calls: Array<{ init: RequestInit | undefined }> = [];
    const originalFetch = globalThis.fetch;
    globalThis.fetch = ((_input: unknown, init?: RequestInit) => {
      calls.push({ init });
      return Promise.resolve(
        new Response(JSON.stringify({ instances: [] }), {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
      );
    }) as typeof fetch;
    try {
      const client = new InstancesClient("http://instances.test");
      await client.list();
      await client.stop("x");
    } finally {
      globalThis.fetch = originalFetch;
    }
    assertEquals(calls.length, 2);
    // Both exchanges ride the session cookie.
    for (const { init } of calls) assertEquals(init?.credentials, "include");
    // GET carries no marker; the POST does (cookie-channel CSRF pillar).
    const headers = (call: { init: RequestInit | undefined }) =>
      (call.init?.headers ?? {}) as Record<string, string>;
    assertEquals(headers(calls[0])["x-requested-with"], undefined);
    assertEquals(headers(calls[1])["x-requested-with"], "kallip");
  },
);

// A mixed-version pair (a daemon newer or older than this UI) may carry a
// state token this build never wrote; upstream lands it as "unknown". The
// client passes it through — one degraded instance, not a failed page.
Deno.test(
  "list tolerates an unknown state token from a mixed-version daemon",
  async () => {
    const originalFetch = globalThis.fetch;
    globalThis.fetch = ((_input: unknown, _init?: RequestInit) =>
      Promise.resolve(
        new Response(
          JSON.stringify({
            instances: [
              {
                slug: "team",
                instance_id: "id",
                workspace: "/ws",
                running: true,
                state: "unknown",
                owner: 1000,
              },
            ],
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        ),
      )) as typeof fetch;
    try {
      const client = new InstancesClient("http://instances.test");
      const instances = await client.list();
      assertEquals(instances[0].state, "unknown");
      // Liveness decisions key on `running`, which stays truthful.
      assertEquals(instances[0].running, true);
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);
