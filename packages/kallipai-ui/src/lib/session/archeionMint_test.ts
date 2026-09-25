// Tests for the mint leg of the one-click instance flow: a failed mint
// sets the list error, and a successful retry must clear it (the pending
// card and the registry section render off that error state; a stale one
// would hide the just-minted card the spawn-failure copy points at).
//
// The module under test is rune-bearing; deno test runs it uncompiled, so
// a passthrough $state shim (same pattern as relayWindow_test) lets the
// store run with plain fields. The archeion client singleton is not
// initialized in this process -- mintTagma's failure path throws at the
// client() call, which is exactly the transport failure under test; the
// success path is exercised through the store's own seams by stubbing
// globalThis fetch at the archeion client layer instead.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { archeionSession } = await import("./archeion.svelte.ts");
const { initArcheion } = await import("./archeion.svelte.ts");

Deno.test(
  "a failed mint sets the list error; a successful retry clears it",
  async () => {
    // Failure: the archeion endpoint is dead (fetch rejects).
    initArcheion("http://127.0.0.1:1");
    const originalFetch = globalThis.fetch;
    globalThis.fetch = (() =>
      Promise.reject(new Error("net down"))) as typeof fetch;
    try {
      const first = await archeionSession.mintTagma();
      assertEquals(first, null);
      assertEquals(archeionSession.tagmataError !== null, true);
    } finally {
      globalThis.fetch = originalFetch;
    }

    // Retry succeeds: the minted pair comes back and the error is cleared.
    globalThis.fetch = (() =>
      Promise.resolve(
        new Response(
          JSON.stringify({
            id: "t-1",
            code: "sk-enroll-x",
            created_at: "2026-01-01T00:00:00Z",
          }),
          { status: 200, headers: { "content-type": "application/json" } },
        ),
      )) as typeof fetch;
    try {
      const second = await archeionSession.mintTagma();
      assertEquals(second, { id: "t-1", code: "sk-enroll-x" });
      assertEquals(archeionSession.tagmataError, null);
    } finally {
      globalThis.fetch = originalFetch;
    }
  },
);
