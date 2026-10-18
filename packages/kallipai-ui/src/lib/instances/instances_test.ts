// The deployment gate on the instances store: a deployment that declares no
// instance host never asks for the service. The store is where that is
// observable -- zero requests, no poll armed, no error state -- which is what
// keeps the /tagmata page quiet instead of reporting on a service the
// deployment does not serve. The poller is a third path to the wire, so its
// switch is pinned through a timer stub rather than through a clock.
//
// The store module is rune-bearing, but deno test runs it uncompiled, so a
// passthrough $state shim lets it run with plain fields (the
// gateway/store_test pattern). Assertions face the observable store state,
// and every case sets its own declaration and resets the state it reads, so
// the file is order-independent.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const client = await import("./client.ts");
const { instancesStore } = await import("./instances.svelte.ts");

const BASE = "http://instances.test";

/** One refresh: both faces plus the floating capability probe. */
const REFRESH_PATHS = [
  BASE + "/health",
  BASE + "/list",
  BASE + "/capabilities",
];

/** Answer every request with an empty-but-well-formed body, recording the
 * URLs the store reaches for. */
function countingFetch(): { calls: string[]; restore: () => void } {
  const calls: string[] = [];
  const original = globalThis.fetch;
  globalThis.fetch = ((input: unknown) => {
    calls.push(String(input));
    return Promise.resolve(
      new Response(JSON.stringify({ instances: [], methods: [] }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
    );
  }) as typeof fetch;
  return {
    calls,
    restore: () => (globalThis.fetch = original),
  };
}

/** Swap the timer pair for counters: a case can then see whether a poll was
 * armed at all, and fire the armed callback on demand. No clock involved. */
function stubbedTimers(): {
  armed: Array<() => void>;
  stats: { cleared: number };
  restore: () => void;
} {
  const armed: Array<() => void> = [];
  const stats = { cleared: 0 };
  const originalSet = globalThis.setInterval;
  const originalClear = globalThis.clearInterval;
  let nextId = 1;
  globalThis.setInterval = ((fn: () => void) => {
    armed.push(fn);
    return nextId++;
  }) as unknown as typeof setInterval;
  globalThis.clearInterval = ((_id: number) => {
    stats.cleared += 1;
  }) as unknown as typeof clearInterval;
  return {
    armed,
    stats,
    restore: () => {
      globalThis.setInterval = originalSet;
      globalThis.clearInterval = originalClear;
    },
  };
}

Deno.test("no instance host means no request and no poll", async () => {
  const probe = countingFetch();
  const timers = stubbedTimers();
  client.initInstances(BASE, { instancesServiceEnabled: false });
  // The store is a module singleton: these two bits are this case's to read,
  // so it clears them first instead of inheriting a neighbour's.
  instancesStore.loaded = false;
  instancesStore.errorKind = null;
  try {
    await instancesStore.refresh();
    await instancesStore.fetchCapabilities();
    instancesStore.startPolling(10);
    assertEquals(timers.armed.length, 0);
  } finally {
    instancesStore.stopPolling();
    timers.restore();
    probe.restore();
    client.initInstances(BASE);
  }
  assertEquals(probe.calls, []);
  assertEquals(instancesStore.errorKind, null);
  assertEquals(instancesStore.loaded, false);
});

Deno.test(
  "an unset declaration reaches for the three refresh paths",
  async () => {
    const probe = countingFetch();
    client.initInstances(BASE);
    try {
      await instancesStore.refresh();
    } finally {
      probe.restore();
    }
    assertEquals(probe.calls, REFRESH_PATHS);
    assertEquals(instancesStore.errorKind, null);
    assertEquals(instancesStore.loaded, true);
  },
);

Deno.test("the poller arms one interval and drops it on stop", async () => {
  const probe = countingFetch();
  const timers = stubbedTimers();
  client.initInstances(BASE);
  try {
    instancesStore.startPolling(10);
    assertEquals(timers.armed.length, 1);
    // The poll's own first refresh is in flight; buffer it, then fire the
    // arm and read what a tick alone reaches for.
    await instancesStore.refresh();
    probe.calls.length = 0;
    timers.armed[0]();
    await new Promise((resolve) => setTimeout(resolve, 0));
    assertEquals(probe.calls, REFRESH_PATHS);
    instancesStore.stopPolling();
    assertEquals(timers.stats.cleared, 1);
  } finally {
    instancesStore.stopPolling();
    timers.restore();
    probe.restore();
  }
});
