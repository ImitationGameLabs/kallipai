// The status snapshot mapping in RealtimeStore: the wire payload (snake_case,
// `token_budget_unlimited` skipped when the tagma's budget is bounded) becomes
// the camelCase summary the dashboard card and the channel header read. The
// absent key is the production shape -- the wire type skips it when false --
// so a wrong default would label every bounded tagma unlimited. Rune-bearing
// module under `deno test`: the passthrough $state shim is installed before the
// dynamic import (the statusCard_test pattern), which is why the imports below
// are dynamic rather than static.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { realtimeStore } = await import("./realtime.svelte.ts");

/** The payload a bounded tagma sends: no `token_budget_unlimited` key. */
const bounded = {
  root_state: "idle",
  subagents_total: 3,
  subagents_active: 1,
  token_budget: 1000,
  token_consumed: 25,
} as const;

Deno.test("an absent unlimited key reads as a bounded budget", () => {
  const id = "tagma-bounded";
  realtimeStore.ingestStatusSnapshot(id, { ...bounded });

  const summary = realtimeStore.statusFor(id);
  assertEquals(summary?.tokenBudgetUnlimited, false);
  // The rest of the payload rides through: the default is the only change.
  assertEquals(summary?.subagentsActive, 1);
  assertEquals(summary?.subagentsTotal, 3);
  assertEquals(summary?.tokenBudget, 1000);
  assertEquals(summary?.tokenConsumed, 25);
});

Deno.test("an explicit unlimited key reaches the summary", () => {
  const id = "tagma-unlimited";
  realtimeStore.ingestStatusSnapshot(id, {
    ...bounded,
    token_budget_unlimited: true,
  });

  assertEquals(realtimeStore.statusFor(id)?.tokenBudgetUnlimited, true);
});

Deno.test("a null payload keeps the last snapshot", () => {
  const id = "tagma-keep-last";
  realtimeStore.ingestStatusSnapshot(id, {
    ...bounded,
    token_budget_unlimited: true,
  });
  const before = realtimeStore.statusFor(id);
  assertEquals(before?.tokenBudgetUnlimited, true);

  // The focus-refresh GET answers null for a tagma that never broadcast:
  // keep the last snapshot rather than evicting it.
  realtimeStore.ingestStatusSnapshot(id, null);

  // The snapshot read before the null answer is the store's live object, so
  // it can be rewritten in place: the expectation is written out fresh, and
  // the full-shape comparison is deliberate. A field added to the summary
  // means a line added here.
  assertEquals(realtimeStore.statusFor(id), {
    rootState: "idle",
    subagentsTotal: 3,
    subagentsActive: 1,
    tokenBudget: 1000,
    tokenConsumed: 25,
    tokenBudgetUnlimited: true,
  });
  // A null answer stores nothing of its own: a tagma that never reported
  // still reads unknown after answering null.
  const fresh = "tagma-never-reported";
  realtimeStore.ingestStatusSnapshot(fresh, null);
  assertEquals(realtimeStore.statusFor(fresh), undefined);
});
