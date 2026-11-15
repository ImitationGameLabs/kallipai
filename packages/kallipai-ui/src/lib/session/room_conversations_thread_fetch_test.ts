// Behaviour of the discussion read: how it pages, when it may call itself
// whole, and what it keeps. The client is a structural fake (no wire), and a
// passthrough `$state` shim lets the rune-bearing store module run uncompiled
// -- the same harness the other store tests use.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { encodeB64 } = await import("@kallipai/kallipai-common");
const { initRelays, lescheClientOrFail } = await import("./archeion.svelte.ts");
const { encodeRoomSendMessage } = await import("../room-message.ts");
const { roomConversationsStore } =
  await import("./roomConversations.svelte.ts");

/** The page ceiling the read uses: a shorter page ends the walk. */
const PAGE = 200;

interface Row {
  readonly seq: number;
  readonly ciphertext: string;
  readonly sender: { id: string; kind: "human"; handle: string };
  readonly created_at: string;
  readonly thread: number | null;
}

function row(seq: number, thread?: number): Row {
  const payload = encodeRoomSendMessage(`line ${seq}`, {
    ...(thread !== undefined ? { thread } : {}),
  });
  return {
    seq,
    ciphertext: encodeB64(new TextEncoder().encode(payload)),
    sender: { id: "u1", kind: "human", handle: "@ada" },
    created_at: "2026-10-21T00:00:00Z",
    thread: thread ?? null,
  };
}

interface Call {
  readonly afterSeq?: number;
  readonly thread?: number;
  readonly limit?: number;
}

let calls: Call[] = [];
let pages: Row[][] = [];

initRelays("http://example.invalid", "http://example.invalid");
const client = lescheClientOrFail() as unknown as {
  fetchRoomMessages: (
    roomId: string,
    options: { afterSeq?: number; thread?: number; limit?: number },
  ) => Promise<Row[]>;
};
client.fetchRoomMessages = (_roomId, options) => {
  calls.push(options);
  return Promise.resolve(pages.shift() ?? []);
};

function reset(staged: Row[][]): void {
  calls = [];
  pages = staged;
}

Deno.test(
  "one page names the discussion and the cursor it starts from",
  async () => {
    // A full page says the discussion goes on, and its cursor is the last row it
    // carried: the next page is asked for from there.
    const first = [
      ...Array.from({ length: PAGE }, (_, i) => {
        const seq = i + 1;
        return seq === 5 ? row(seq, 1) : row(seq);
      }),
    ];
    reset([first]);
    const page = await roomConversationsStore.fetchThreadPage("room-1", 1, 0);
    assertEquals(
      page.lines.map((line) => line.seq),
      [1, 5],
    );
    assertEquals(page.lastSeq, PAGE);
    assertEquals(page.hasMore, true);
    assertEquals(calls, [{ afterSeq: 0, thread: 1, limit: PAGE }]);
  },
);

Deno.test("a short page is the end of the discussion", async () => {
  reset([[row(1), row(2, 1), row(3, 9)]]);
  const page = await roomConversationsStore.fetchThreadPage("room-1", 1, PAGE);
  assertEquals(
    page.lines.map((line) => line.seq),
    [1, 2],
  );
  assertEquals(page.lastSeq, 3);
  assertEquals(page.hasMore, false);
  assertEquals(calls, [{ afterSeq: PAGE, thread: 1, limit: PAGE }]);
});

Deno.test("the cursor moves past a row the filter dropped", async () => {
  // A relay that predates the `thread` parameter answers with room rows: the
  // stored-root filter drops them here, but the page still knows where it got
  // to. A cursor read off the kept lines would ask for the same stretch again
  // and the walk would never move.
  reset([[row(10, 1), row(11), row(12)]]);
  const page = await roomConversationsStore.fetchThreadPage("room-1", 1, 9);
  assertEquals(
    page.lines.map((line) => line.seq),
    [10],
  );
  assertEquals(page.lastSeq, 12);
});

Deno.test(
  "a row that does not decode loses the row, not the page",
  async () => {
    const broken = { ...row(11, 1), ciphertext: "not base64 at all" };
    reset([[row(10, 1), broken, row(12, 1)]]);
    const page = await roomConversationsStore.fetchThreadPage("room-1", 1, 9);
    assertEquals(
      page.lines.map((line) => line.seq),
      [10, 12],
    );
    assertEquals(page.hasMore, false);
  },
);

Deno.test("a failed request is an error, never an empty end", async () => {
  // An empty page carries `hasMore: false`, which the panel reads as the end of
  // the discussion. A request that failed must stay an error, or the reader is
  // told the discussion ends where the network dropped.
  reset([]);
  const answer = client.fetchRoomMessages;
  client.fetchRoomMessages = () => Promise.reject(new Error("offline"));
  let threw = false;
  try {
    await roomConversationsStore.fetchThreadPage("room-1", 1, 0);
  } catch {
    threw = true;
  } finally {
    client.fetchRoomMessages = answer;
  }
  assertEquals(threw, true);
});

Deno.test("a position below one asks for nothing", async () => {
  // A root below 1 names no row -- seqs start at 1 and a stored root is a seq --
  // so an empty discussion is what the server would answer; the request itself
  // would be refused, so none is sent.
  reset([]);
  const none = await roomConversationsStore.fetchThreadPage("room-1", 0, -1);
  assertEquals(none.lines, []);
  assertEquals(none.lastSeq, null);
  assertEquals(none.hasMore, false);

  // Not an integer is not a position either: a guard written as `root < 1`
  // would let NaN through and send a request that can only be refused.
  const nan = await roomConversationsStore.fetchThreadPage(
    "room-1",
    Number.NaN,
    -1,
  );
  assertEquals(nan.lines, []);
  assertEquals(nan.hasMore, false);
  assertEquals(calls.length, 0);
});
