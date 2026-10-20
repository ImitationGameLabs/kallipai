// The degraded offline view's probe: it holds no history source, so every
// probe is a retryable `error` -- never a verdict that the quoted row is gone.
// The head guard is cleared first, so the answer comes from the leaf itself.
//
// The module under test is rune-bearing and `deno test` runs it uncompiled, so
// a passthrough `$state` shim lets it run with plain fields.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { OfflineConversation } = await import("./conversation.svelte.ts");
import { LOCAL_OPERATOR_SENDER } from "../transcript.ts";

/** The offline view over a store that holds nothing for it. */
function offline() {
  const conv = new OfflineConversation(
    "conv-offline",
    { get: () => undefined } as never,
    "tagma-1",
    LOCAL_OPERATOR_SENDER,
  );
  conv.maxRendered = 9;
  return conv;
}

Deno.test(
  "the offline view answers error rather than claiming deletion",
  async () => {
    assertEquals(
      await offline().peekRow(9),
      "error",
      "no history source, so no proof the target is gone",
    );
  },
);
