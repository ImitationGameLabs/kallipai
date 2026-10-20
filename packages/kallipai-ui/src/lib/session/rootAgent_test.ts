// Tests for the root-agent resolution the relay paths share: the manage
// plane's roster answer, read as a roster. Both shapes matter -- a body that
// carries `agents` beside its `upstream_facts` resolves the entry nothing
// created, and a body without that key resolves nothing (the caller reports
// nothing rather than inventing an id). The failure paths -- a roster with no
// rootless entry, a root whose id is unusable, a manage call that throws --
// all resolve nothing rather than guessing. The cache is per channel, and a
// negative answer is not cached. All IO is a fake; no network.

const { assertEquals } = await import("@std/assert");
const { relayRootAgentId } = await import("./rootAgent.ts");

type ManageAnswer = { status: number; body?: unknown };

/** A channel stand-in with just the manage plane the resolver uses. */
function fakeChannel(answers: ManageAnswer[]) {
  const calls: { method: string; path: string }[] = [];
  const channel = {
    calls,
    manage(method: string, path: string) {
      calls.push({ method, path });
      const answer = answers.shift() ?? { status: 500 };
      return Promise.resolve({
        status: answer.status,
        body: answer.body ?? null,
      });
    },
  };
  return channel as unknown as Parameters<typeof relayRootAgentId>[0] &
    typeof channel;
}

Deno.test("the root is the roster entry nothing created", async () => {
  const channel = fakeChannel([
    {
      status: 200,
      body: {
        agents: [
          { id: "child-1", created_by: "root-1" },
          { id: "root-1", created_by: null },
        ],
        upstream_facts: [],
      },
    },
  ]);
  assertEquals(await relayRootAgentId(channel), "root-1");
  assertEquals(channel.calls, [{ method: "GET", path: "/agents" }]);
});

Deno.test("a body without an agents key resolves no root", async () => {
  const channel = fakeChannel([
    {
      status: 200,
      body: { upstream_facts: [{ tagma_id: "t1" }] },
    },
  ]);
  assertEquals(await relayRootAgentId(channel), null);
  assertEquals(channel.calls.length, 1);
});

Deno.test("a rejected roster answer resolves no root", async () => {
  const channel = fakeChannel([{ status: 502, body: { agents: [] } }]);
  assertEquals(await relayRootAgentId(channel), null);
});

Deno.test(
  "a resolved id is cached; a negative answer is asked again",
  async () => {
    const channel = fakeChannel([
      { status: 503 },
      { status: 200, body: { agents: [{ id: "root-1", created_by: null }] } },
    ]);
    // A transient failure must not pin "no root" for the life of the tunnel.
    assertEquals(await relayRootAgentId(channel), null);
    assertEquals(await relayRootAgentId(channel), "root-1");
    assertEquals(channel.calls.length, 2);
    // The resolved id answers every later caller without another roster read.
    assertEquals(await relayRootAgentId(channel), "root-1");
    assertEquals(channel.calls.length, 2);
  },
);

Deno.test(
  "a roster whose entries all have a creator resolves no root",
  async () => {
    const channel = fakeChannel([
      {
        status: 200,
        body: {
          agents: [
            { id: "a-1", created_by: "root-1" },
            { id: "a-2", created_by: "a-1" },
          ],
        },
      },
    ]);
    assertEquals(await relayRootAgentId(channel), null);
  },
);

Deno.test("a root entry with an unusable id resolves no root", async () => {
  const channel = fakeChannel([
    {
      status: 200,
      body: { agents: [{ id: 7, created_by: null }, { created_by: null }] },
    },
  ]);
  assertEquals(await relayRootAgentId(channel), null);
});

Deno.test("a manage call that throws resolves no root, uncached", async () => {
  // A closed tunnel rejects instead of answering: the resolver reports no
  // root, and the answer is not cached, so the next caller asks again.
  let calls = 0;
  const channel = {
    manage() {
      calls += 1;
      return Promise.reject(new Error("channel closed"));
    },
  } as unknown as Parameters<typeof relayRootAgentId>[0];
  assertEquals(await relayRootAgentId(channel), null);
  assertEquals(await relayRootAgentId(channel), null);
  assertEquals(calls, 2);
});
