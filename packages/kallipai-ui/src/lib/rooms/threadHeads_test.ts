// The thread head read: the conditions a fetched row must meet before its words
// become a thread's name, and the position the read asks for. A head that fails
// any of them leaves the room's name alone -- the notification is sent either
// way, so a wrong name is worse than none.
//
// The read is injected, so nothing here reaches the network; the passthrough
// `$state` shim lets the rune-bearing module run uncompiled (the store-test
// harness).

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { encodeB64 } = await import("@kallipai/kallipai-common");
const { encodeRoomSendMessage } = await import("../room-message.ts");
const { rememberThreadHead, resolveThreadHead, threadHeadOf } =
  await import("./threadHeads.svelte.ts");

interface Row {
  readonly seq: number;
  readonly ciphertext: string;
  readonly epoch: number;
  readonly sender: { id: string; kind: "human"; handle: string };
  readonly created_at: string;
  readonly thread?: number | null;
}

function row(seq: number, text: string, thread?: number): Row {
  const payload = encodeRoomSendMessage(
    text,
    thread === undefined ? {} : { thread },
  );
  return {
    seq,
    ciphertext: encodeB64(new TextEncoder().encode(payload)),
    epoch: 1,
    sender: { id: "u1", kind: "human", handle: "@ada" },
    created_at: "2026-11-17T00:00:00Z",
    thread: thread ?? null,
  };
}

interface Ask {
  readonly roomId: string;
  readonly options: { beforeSeq?: number; limit?: number };
}

function reader(rows: readonly Row[], asks: Ask[] = []) {
  return (roomId: string, options: { beforeSeq?: number; limit?: number }) => {
    asks.push({ roomId, options });
    return Promise.resolve(rows);
  };
}

/** A room that answers each position with the row at it, the way the route
 *  does: the page below `beforeSeq` ends at `beforeSeq - 1`. */
function bySeq(rows: readonly Row[], asks: Ask[] = []) {
  return (roomId: string, options: { beforeSeq?: number; limit?: number }) => {
    asks.push({ roomId, options });
    const seq = (options.beforeSeq ?? 0) - 1;
    return Promise.resolve(rows.filter((row) => row.seq === seq));
  };
}

Deno.test("a remembered head is not read again", async () => {
  rememberThreadHead("room-1", 5, "the first words");
  const asks: Ask[] = [];
  const head = await resolveThreadHead(
    "room-1",
    5,
    reader([row(5, "ignored")], asks),
  );
  assertEquals(head, "the first words");
  assertEquals(asks, []);
});

Deno.test("the read asks for the row just below the root", async () => {
  // The backward page is the one whose meaning ("the newest rows below this
  // position") the route states, so `before_seq = root + 1` answers with the
  // root itself and keeps doing so whatever the forward page becomes.
  const asks: Ask[] = [];
  const head = await resolveThreadHead(
    "room-2",
    5,
    reader([row(5, "hello")], asks),
  );
  assertEquals(asks, [
    { roomId: "room-2", options: { beforeSeq: 6, limit: 1 } },
  ]);
  assertEquals(head, "hello");
  assertEquals(threadHeadOf("room-2", 5), "hello");
});

Deno.test("a declaration that names a reply still finds the head", async () => {
  // A sender declares the thread it writes into, and the declaration may name
  // any row of it. The fetched row carries the head's position in its own
  // `thread`, so the name comes from the head -- never from the reply's words.
  const asks: Ask[] = [];
  const head = await resolveThreadHead(
    "room-3",
    5,
    bySeq([row(5, "replying", 3), row(3, "the head words")], asks),
  );
  assertEquals(head, "the head words");
  assertEquals(asks, [
    { roomId: "room-3", options: { beforeSeq: 6, limit: 1 } },
    { roomId: "room-3", options: { beforeSeq: 4, limit: 1 } },
  ]);
  // The name is recorded under the head, so the trail of the discussion reads
  // it; the reply's own position names nothing.
  assertEquals(threadHeadOf("room-3", 3), "the head words");
  assertEquals(threadHeadOf("room-3", 5), null);
});

Deno.test(
  "a head that names another row is refused, not followed",
  async () => {
    // The store normalizes every reply's root to the head's position, so a head
    // whose `thread` is not empty is not a shape this log writes. Trusting it
    // would follow a pointer this module cannot bound.
    const asks: Ask[] = [];
    const head = await resolveThreadHead(
      "room-8",
      5,
      bySeq([row(5, "replying", 3), row(3, "still a reply", 1)], asks),
    );
    assertEquals(head, null);
    assertEquals(asks.length, 2);
    assertEquals(threadHeadOf("room-8", 3), null);
  },
);

Deno.test("a read that never answers leaves the room's name", async () => {
  // The client allows a request 30 seconds and the notification is already
  // waiting, so one head read gives up long before that.
  let stalled = 0;
  const stuck = () => {
    stalled += 1;
    return new Promise<readonly Row[]>(() => {});
  };
  const started = Date.now();
  assertEquals(await resolveThreadHead("room-9", 5, stuck, 10), null);
  assertEquals(
    Date.now() - started < 1_000,
    true,
    "the deadline ended the wait",
  );
  assertEquals(stalled, 1);
});

Deno.test("a row at another position is dropped, not trusted", async () => {
  const head = await resolveThreadHead("room-4", 5, reader([row(7, "other")]));
  assertEquals(head, null);
  assertEquals(threadHeadOf("room-4", 5), null);
});

Deno.test("a failed read and an empty answer both answer null", async () => {
  const failing = () => Promise.reject(new Error("offline"));
  assertEquals(await resolveThreadHead("room-5", 5, failing), null);
  assertEquals(await resolveThreadHead("room-6", 5, reader([])), null);
  assertEquals(threadHeadOf("room-5", 5), null);
  assertEquals(threadHeadOf("room-6", 5), null);
});
