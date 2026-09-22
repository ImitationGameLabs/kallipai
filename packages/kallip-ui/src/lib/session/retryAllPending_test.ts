// Tests for the durable pending flush wired by task 35: the reconnect
// auto-flush that drains persisted unsent lines once a transport is live.
//
// `deno test` has no IndexedDB global. Under that, cache.ts's
// readPendingByTagma honors its contract (an internal try/catch returns
// an empty array, same as readTail/readTailBefore), so retryAllPending
// silently no-ops; the conversation-level catch is defense in depth
// mirroring the channels-side double catch (currently unreachable).
// What the test locks is the end-to-end behavior: with no IDB, a
// caller of retryAllPending never sees a rejection. The queued-line
// idempotence guard gets the same no-IDB treatment: a line already in
// the pending queue is not re-queued.
//
// Harness mirrors channels_refresh_test: a passthrough $state shim lets the
// rune-bearing module run uncompiled, and the transport is a structural
// fake (the paths under test never touch the wire).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

type Transport = import("./transport.ts").Transport;
const { assertEquals } = await import("@std/assert");
const { RelayConversation } = await import("./conversation.svelte.ts");
const { LOCAL_OPERATOR_SENDER, sendFailed, withUserLine } =
  await import("../transcript.ts");

/** Minimal transport surface (same shape as channels_refresh_test's). */
const fakeTransport: Transport = {
  localSender: { kind: "user", id: "u-1", handle: "alice" },
  async *replies() {},
  async *signals() {},
  async *status() {},
  send() {
    return Promise.resolve();
  },
  close() {},
} as unknown as Transport;

function makeConv(tagmaId: string) {
  // Second parameter is the lesche client (null): the retry paths under
  // test never reach the wire.
  return new RelayConversation(
    "conv-1",
    null as never,
    fakeTransport,
    tagmaId,
    "T",
  );
}

Deno.test(
  "retryAllPending skips silently when IndexedDB is unavailable",
  async () => {
    const conv = makeConv("tagma-1");
    // No IDB global under deno test: readPendingByTagma returns [] per
    // its contract, so the flush no-ops -- and the call never rejects.
    await conv.retryAllPending();
    assertEquals(conv.pending.length, 0);
  },
);

Deno.test(
  "retrySend is a no-op for a line already in the pending queue",
  () => {
    const conv = makeConv("tagma-2");
    let t = withUserLine(conv.transcript, "hello", 42, LOCAL_OPERATOR_SENDER);
    t = sendFailed(t, 42, "boom");
    conv.transcript = t;
    conv.pending = [{ localId: 42, text: "hello", attachment: undefined }];
    // The queued-line guard returns before the pump: no double queue entry.
    conv.retrySend(42);
    assertEquals(conv.pending.length, 1);
    assertEquals(conv.pending[0].localId, 42);
  },
);
