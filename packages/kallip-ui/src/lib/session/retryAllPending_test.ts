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
const assertMod = await import("@std/assert");
const { assertEquals } = assertMod;
const assertNotIncludesId = (pending: { localId: number }[], id: number) => {
  assertEquals(
    pending.some((p) => p.localId === id),
    false,
  );
};
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

Deno.test(
  "retryAllPending renders mixed rows in stored order; failed never enqueued",
  async () => {
    const conv = makeConv("tagma-order");
    // Injected stored order (oldest-first, as readPendingByTagma returns):
    // a queued row, then a failed one, then another queued row. The mixed
    // timing is the point -- reordering these would scramble the bubbles.
    const stored = [
      {
        tagmaId: "tagma-order",
        localSeq: -300,
        text: "first queued",
        createdAt: "2026-09-23T00:00:01Z",
        status: "queued" as const,
      },
      {
        tagmaId: "tagma-order",
        localSeq: -301,
        text: "second failed",
        createdAt: "2026-09-23T00:00:02Z",
        status: "failed" as const,
        lastError: "boom",
      },
      {
        tagmaId: "tagma-order",
        localSeq: -302,
        text: "third queued",
        createdAt: "2026-09-23T00:00:03Z",
        status: "queued" as const,
      },
    ];
    const { setPendingRowsForTests } =
      await import("@kallipai/kallip-lesche-client");
    setPendingRowsForTests(() => stored);
    try {
      await conv.retryAllPending();
    } finally {
      setPendingRowsForTests(null);
    }
    const user = conv.transcript.lines.filter((l) => l.role === "user");
    // Stored order == rendered order (single-loop, per-row branch).
    assertEquals(
      user.map((l) => l.historyId),
      [-300, -301, -302],
    );
    // Shapes: queued rows render as in-flight sends, the failed row keeps
    // its failure shape.
    assertEquals(user[0]!.status, "sending");
    assertEquals(user[1]!.status, "failed");
    assertEquals(user[2]!.status, "sending");
    // The failed row is not enqueued: only the queued ones auto-send (the
    // pump may already have shifted the first queued row out by the time we
    // assert -- the race is fine, the failed id must simply never appear).
    assertNotIncludesId(conv.pending, -301);
  },
);
