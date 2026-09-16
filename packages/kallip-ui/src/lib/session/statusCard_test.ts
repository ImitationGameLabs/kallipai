// Tests for the status-card store's event wiring: attach() pulls the
// roster through the conversation backend once, nudge() re-pulls without
// waiting for the reconciliation poll (this is the relay `tagma_status` /
// direct-SSE event path the chat page feeds), overlapping nudges collapse
// into one request, and detach() clears rows and stops further pulls.
// Rune-bearing module under `deno test`: passthrough $state shim (the
// channels_openBudget_test pattern). The seam is OfflineBackend over a stub
// TagmaClient, so the real adapter runs and only the HTTP client is faked.
// The store's own reconciliation intervals (15s/30s) never fire inside a
// test; no `document` is installed, so startVisibleInterval degrades to a
// bare timer that detach() always clears.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

import type {
  ListAgentsManagementResponse,
  TagmaClient,
  WireAgentManagementSummary,
} from "@kallipai/kallip-client";

const { assertEquals } = await import("@std/assert");
import type * as stdAssert from "@std/assert";

const assertStrictEquals: typeof stdAssert.assertStrictEquals = (
  await import("@std/assert")
).assertStrictEquals;
const { OfflineBackend } = await import("../manage/backend.ts");
const assert: typeof stdAssert.assert = (await import("@std/assert")).assert;
const { statusCardStore } = await import("./statusCard.svelte.ts");

const root: WireAgentManagementSummary = {
  id: "root-1",
  workspace_root: "/w",
  state: "busy",
  created_by: null,
  role: "root",
  description: "",
  activity: "thinking",
  duty: "onduty",
  faulted_reason: null,
  conversation_id: null,
  profile_set: null,
};

const sub: WireAgentManagementSummary = {
  ...root,
  id: "sub-1",
  created_by: "root-1",
  state: "idle",
  role: "worker",
  activity: "",
};

/** Only the calls the store makes from attach()/nudge(): the roster pull,
 * and the registry pull whose failure is a documented non-fatal path
 * (denominators stay null). getAgentStatus (the first-pull and the slow
 * poll both use it) rejects, so the context merge's failure paths run. */
class StubClient {
  rosterCalls = 0;
  statusCalls = 0;
  /** Mutable so a test can flip an observable field between refreshes
   * (the fixture rows are readonly). */
  rootActivity = "thinking";
  listAgents(): Promise<ListAgentsManagementResponse> {
    this.rosterCalls++;
    return Promise.resolve({
      agents: [{ ...root, activity: this.rootActivity }, sub],
    });
  }
  getAgentStatus(): Promise<never> {
    this.statusCalls++;
    return Promise.reject(new Error("no status in test"));
  }
  getProfiles(): Promise<never> {
    return Promise.reject(new Error("registry unavailable in test"));
  }
}

const flush = () => new Promise((r) => setTimeout(r, 20));
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const backend = (stub: StubClient) =>
  new OfflineBackend(stub as unknown as TagmaClient);

Deno.test("attach pulls the roster once through the backend", async () => {
  const stub = new StubClient();
  try {
    statusCardStore.attach(backend(stub));
    await flush();
    assertEquals(stub.rosterCalls, 2);
    assertEquals(statusCardStore.rootRow?.id, "root-1");
    assertEquals(statusCardStore.subRows.length, 1);
    assertEquals(statusCardStore.subRows[0]?.id, "sub-1");
  } finally {
    statusCardStore.detach();
  }
});

Deno.test(
  "nudge re-pulls the roster without waiting for the poll",
  async () => {
    const stub = new StubClient();
    try {
      statusCardStore.attach(backend(stub));
      await flush();
      assertEquals(stub.rosterCalls, 2);
      statusCardStore.nudge();
      await flush();
      assertEquals(stub.rosterCalls, 3);
      // Two agents x (first pull + one cooldown-sealed gap retry) = 4
      // calls total; a growing count would mean roster<->gap recursion.
      assertEquals(stub.statusCalls, 4);
    } finally {
      statusCardStore.detach();
    }
  },
);

Deno.test("overlapping nudges collapse into one request", async () => {
  const stub = new StubClient();
  try {
    statusCardStore.attach(backend(stub));
    await flush();
    assertEquals(stub.rosterCalls, 2);
    statusCardStore.nudge();
    statusCardStore.nudge(); // second lands while the first pull is in flight
    await flush();
    assertEquals(stub.rosterCalls, 3);
  } finally {
    statusCardStore.detach();
  }
});

Deno.test("detach clears rows and makes nudges no-ops", async () => {
  const stub = new StubClient();
  statusCardStore.attach(backend(stub));
  await flush();
  statusCardStore.detach();
  assertEquals(statusCardStore.rootRow, null);
  assertEquals(statusCardStore.subRows.length, 0);
  statusCardStore.nudge();
  await flush();
  assertEquals(stub.rosterCalls, 2);
});

Deno.test("suspend keeps the cache and re-attach paints from it", async () => {
  const stub = new StubClient();
  try {
    statusCardStore.attach(backend(stub));
    await flush();
    statusCardStore.suspend();
    // The warm cache survives the page-unmount teardown.
    assertEquals(statusCardStore.rootRow?.id, "root-1");
    assertEquals(statusCardStore.subRows.length, 1);
    const rosterBefore = stub.rosterCalls;
    statusCardStore.attach(backend(stub));
    await flush();
    // Re-attach painted the cached rows again, then refreshed.
    assertEquals(statusCardStore.rootRow?.id, "root-1");
    assertEquals(stub.rosterCalls > rosterBefore, true);
  } finally {
    statusCardStore.detach();
  }
});

/** Regression nail for the production refresh storm: a same-data roster
 * refresh must keep the rootRow object identity. Svelte skips a
 * same-reference $state write, so identity preservation is exactly what
 * stops effects that read rootRow (the chat page's attach effect) from
 * re-running per frame -- each re-run tore down the projection feed and
 * re-dialed the SSE stream. The passthrough $state shim makes the
 * identity assertion the same check Svelte's Object.is short-circuit
 * makes at runtime. */
Deno.test(
  "same-data roster refreshes keep the rootRow identity stable",
  async () => {
    const stub = new StubClient();
    try {
      statusCardStore.attach(backend(stub));
      await flush();
      const before = statusCardStore.rootRow;
      assertEquals(before?.id, "root-1");
      statusCardStore.nudge();
      await flush();
      statusCardStore.nudge();
      await flush();
      // Two same-data refreshes later: same object, no effect churn.
      assertStrictEquals(statusCardStore.rootRow, before);
    } finally {
      statusCardStore.detach();
    }
  },
);

Deno.test("a changed roster swaps the rootRow reference", async () => {
  const stub = new StubClient();
  try {
    statusCardStore.attach(backend(stub));
    await flush();
    const before = statusCardStore.rootRow;
    stub.rootActivity = "dispatching"; // observable change
    statusCardStore.nudge();
    await flush();
    const after = statusCardStore.rootRow;
    assertEquals(after?.activity, "dispatching");
    assertEquals(after === before, false);
  } finally {
    stub.rootActivity = "thinking";
    statusCardStore.detach();
  }
});

// --- Online projection-feed backstop (dirty SSE + visible intervals) -------

/** Minimal ProjectionFeed stub: captures the dirty callback and fires it on
 * demand (or never, standing in for a silently dead stream). */
class StubFeed {
  onDirty: (() => void) | null = null;
  subscribe(onDirty: () => void): () => void {
    this.onDirty = onDirty;
    return () => {
      this.onDirty = null;
    };
  }
  fire(): void {
    this.onDirty?.();
  }
}

/** The real Offline adapter over the stub client with a projectionFeed
 * grafted on -- exactly what the Online path looks like to attach(). */
const feedBackend = (stub: StubClient, feed: StubFeed) => {
  const b = new OfflineBackend(stub as unknown as TagmaClient);
  return Object.assign(b, { projectionFeed: feed });
};

Deno.test("a silent feed's backstop keeps the roster refreshing", async () => {
  const stub = new StubClient();
  const feed = new StubFeed(); // never fires: the SSE is dead
  try {
    statusCardStore.attach(feedBackend(stub, feed), 20);
    await flush();
    const afterAttach = stub.rosterCalls;
    // No dirty frames at all: only the 20ms backstop intervals advance
    // the counters. The roster poll (and the context poll via its own
    // cooldown path) must keep running without any feed activity.
    await sleep(70);
    assert(
      stub.rosterCalls > afterAttach,
      `backstop must poll a dead feed (attach=${afterAttach}, now=${stub.rosterCalls})`,
    );
  } finally {
    statusCardStore.detach();
  }
});

Deno.test(
  "dirty fires during an in-flight roster collapse into it",
  async () => {
    const stub = new StubClient();
    const feed = new StubFeed();
    let release: () => void = () => {};
    const gate = new Promise<void>((r) => {
      release = r;
    });
    const orig = stub.listAgents.bind(stub);
    stub.listAgents = () => {
      stub.rosterCalls++;
      return stub.rosterCalls === 1 ? gate.then(() => orig()) : orig();
    };
    try {
      statusCardStore.attach(feedBackend(stub, feed), 20);
      await flush();
      // The first roster call is still in-flight (gated): the dirty fire
      // and every backstop tick in this window must collapse into it.
      assertEquals(stub.rosterCalls, 1);
      feed.fire();
      await sleep(60);
      assertEquals(
        stub.rosterCalls,
        1,
        "overlapping dirty/backstop pulls must not stack",
      );
    } finally {
      release();
      statusCardStore.detach();
    }
  },
);

Deno.test("suspend stops the feed backstop timer", async () => {
  const stub = new StubClient();
  const feed = new StubFeed();
  statusCardStore.attach(feedBackend(stub, feed), 20);
  await flush();
  statusCardStore.suspend();
  const frozen = stub.rosterCalls;
  await sleep(60);
  assertEquals(
    stub.rosterCalls,
    frozen,
    "suspend must stop the backstop interval",
  );
});

Deno.test(
  "the aggregate mirror follows setSummary and clears on detach",
  () => {
    statusCardStore.setSummary({
      rootState: "busy",
      subagentsTotal: 2,
      subagentsActive: 1,
      tokenBudget: 50_000,
      tokenConsumed: 12_345,
      tokenBudgetUnlimited: false,
    });
    assertEquals(statusCardStore.summary?.tokenConsumed, 12_345);
    assertEquals(statusCardStore.summary?.subagentsActive, 1);
    // An `undefined` write (offline eviction, no data yet) clears it.
    statusCardStore.setSummary(undefined);
    assertEquals(statusCardStore.summary, undefined);
    statusCardStore.setSummary({
      rootState: "idle",
      subagentsTotal: 0,
      subagentsActive: 0,
      tokenBudget: 50_000,
      tokenConsumed: 0,
      tokenBudgetUnlimited: true,
    });
    assertEquals(statusCardStore.summary?.tokenBudgetUnlimited, true);
    // detach() clears every cached value, the mirror included.
    statusCardStore.detach();
    assertEquals(statusCardStore.summary, undefined);
  },
);
