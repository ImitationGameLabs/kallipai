// Behaviour tests for the dead-end guard on /chat/{id}: the "unknown
// conversation" banner belongs to the id the miss was decided for, so moving to
// another conversation must not inherit it. The module is rune-bearing and
// `deno test` runs it uncompiled, so it loads behind a passthrough $state shim.

import { assertEquals } from "@std/assert";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { createUnresolvedGuard } = await import("./unresolvedGuard.svelte.ts");

Deno.test("a miss speaks only for the conversation it was decided for", () => {
  const guard = createUnresolvedGuard();
  assertEquals(guard.unknown("conv-a"), false);

  guard.markUnknown("conv-a");
  assertEquals(guard.unknown("conv-a"), true);
  assertEquals(guard.unknown("conv-b"), false);
});

Deno.test("a later resolution drops the earlier miss", () => {
  const guard = createUnresolvedGuard();
  guard.markUnknown("conv-a");
  guard.settle();

  assertEquals(guard.unknown("conv-a"), false);

  // Coming back to the same id starts undecided again rather than replaying
  // the earlier verdict.
  guard.markUnknown("conv-a");
  assertEquals(guard.unknown("conv-a"), true);
});

Deno.test("the newest miss replaces the older one", () => {
  const guard = createUnresolvedGuard();
  guard.markUnknown("conv-a");
  guard.markUnknown("conv-b");

  assertEquals(guard.unknown("conv-a"), false);
  assertEquals(guard.unknown("conv-b"), true);
});

Deno.test("a cleared record speaks for no conversation at all", () => {
  const guard = createUnresolvedGuard();
  guard.markUnknown("conv-a");
  guard.settle();

  assertEquals(guard.unknown("conv-a"), false);
  assertEquals(guard.unknown("conv-b"), false);
});
