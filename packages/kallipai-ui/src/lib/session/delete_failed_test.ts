// Tests for discarding a failed line (the bubble's delete action): the line
// leaves the transcript, the durable row is removed through the pending store's
// delete seam, and a line that is not failed loses nothing.
//
// Harness mirrors retryAllPending_test: a passthrough $state shim lets the
// rune-bearing module run uncompiled, and the transport is a structural fake
// (none of these paths touch the wire).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

type Transport = import("./transport.ts").Transport;
const { assertEquals } = await import("@std/assert");
const { RelayConversation } =
  await import("./conversation/relayConversation.svelte.ts");
const { LOCAL_OPERATOR_SENDER, sendFailed, withUserLine } =
  await import("../transcript.ts");
const { setPendingDeletesForTests } =
  await import("@kallipai/kallipai-keryx-client");

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
  return new RelayConversation(
    "conv-1",
    null as never,
    fakeTransport,
    tagmaId,
    "T",
  );
}

/** Run `body` with a pending-delete observer installed, and hand back the calls
 *  it saw. The seam is cleared whatever happens, so one test cannot leak into
 *  the next. */
async function withDeletes(
  body: (seen: [string, number][]) => void | Promise<void>,
): Promise<void> {
  const seen: [string, number][] = [];
  setPendingDeletesForTests((tagmaId, localSeq) =>
    seen.push([tagmaId, localSeq]),
  );
  try {
    await body(seen);
  } finally {
    setPendingDeletesForTests(null);
  }
}

Deno.test("discard asks the pending store to drop the row", () =>
  withDeletes((seen) => {
    const conv = makeConv("tagma-del");
    let t = withUserLine(
      conv.transcript.state,
      "hello",
      -7,
      LOCAL_OPERATOR_SENDER,
    );
    t = sendFailed(t, -7, "boom");
    conv.transcript.state = t;

    assertEquals(
      conv.outbox.discard(-7),
      true,
      "the line left, so the answer is yes",
    );

    assertEquals(conv.transcript.rows, []);
    assertEquals(conv.transcript.error, undefined);
    assertEquals(seen, [["tagma-del", -7]]);
  }),
);

Deno.test("discard declines a line that never failed", () =>
  withDeletes((seen) => {
    const conv = makeConv("tagma-sending");
    conv.transcript.state = withUserLine(
      conv.transcript.state,
      "hello",
      -8,
      LOCAL_OPERATOR_SENDER,
    );

    assertEquals(
      conv.outbox.discard(-8),
      false,
      "a line that never failed is not the reader's to take back",
    );

    assertEquals(conv.transcript.rows.length, 1);
    assertEquals(seen, []);
  }),
);

Deno.test("discard drops a re-queued copy from the send queue", () =>
  withDeletes((seen) => {
    const conv = makeConv("tagma-queue");
    let t = withUserLine(
      conv.transcript.state,
      "hello",
      -9,
      LOCAL_OPERATOR_SENDER,
    );
    t = sendFailed(t, -9, "boom");
    conv.transcript.state = t;
    conv.outbox.pending = [
      { localId: -9, text: "hello", attachment: undefined },
    ];

    assertEquals(conv.outbox.discard(-9), true);

    assertEquals(conv.outbox.pending, []);
    assertEquals(seen, [["tagma-queue", -9]]);
  }),
);

Deno.test("discard survives a store that cannot be written", () => {
  // A discard must land even where IndexedDB is missing (private mode): the
  // durable delete is best-effort, the line still leaves the transcript.
  const conv = makeConv("tagma-nostore");
  let t = withUserLine(
    conv.transcript.state,
    "hello",
    -13,
    LOCAL_OPERATOR_SENDER,
  );
  t = sendFailed(t, -13, "boom");
  conv.transcript.state = t;

  assertEquals(
    conv.outbox.discard(-13),
    true,
    "a store that cannot be written still lets the line go",
  );

  assertEquals(conv.transcript.rows, []);
});

Deno.test(
  "a row discarded mid-flush stays gone and its drop is re-issued",
  () =>
    withDeletes(async (seen) => {
      // retryAllPending reads the durable rows, then walks them. A discard that
      // lands inside that read still owns a row in the snapshot, and drawing it
      // back would undo the promise the delete action makes. The flush also
      // re-issues the drop, so a delete that missed heals on the next reconnect.
      const { setPendingRowsForTests } =
        await import("@kallipai/kallipai-keryx-client");
      const conv = makeConv("tagma-race");
      let t = withUserLine(
        conv.transcript.state,
        "hello",
        -12,
        LOCAL_OPERATOR_SENDER,
      );
      t = sendFailed(t, -12, "boom");
      conv.transcript.state = t;
      setPendingRowsForTests(() => [
        {
          tagmaId: "tagma-race",
          localSeq: -12,
          text: "hello",
          createdAt: "2026-09-23T00:00:00Z",
          status: "failed" as const,
          lastError: "boom",
        },
      ]);
      try {
        const flushing = conv.outbox.retryAllPending();
        conv.outbox.discard(-12);
        // The action's own delete lands first; the flush's re-issue follows
        // once the durable read it was already inside lets go.
        assertEquals(seen.length, 1);
        await flushing;
      } finally {
        setPendingRowsForTests(null);
      }

      assertEquals(conv.transcript.rows, []);
      assertEquals(seen, [
        ["tagma-race", -12],
        ["tagma-race", -12],
      ]);
    }),
);

Deno.test("discard declines a line that is still in flight", () => {
  // The POST is under way: the row is not the reader's to take back, and the
  // answer keeps the trigger in place instead of moving focus away from it.
  const conv = makeConv("tagma-inflight");
  let t = withUserLine(
    conv.transcript.state,
    "hello",
    -14,
    LOCAL_OPERATOR_SENDER,
  );
  t = sendFailed(t, -14, "boom");
  conv.transcript.state = t;
  conv.outbox.pendingInFlight = { localId: -14, text: "hello" };

  assertEquals(conv.outbox.discard(-14), false);

  assertEquals(conv.transcript.rows.length, 1, "the row is still there");
});
