// Tests for the relay leaf's lazy-window machinery: the newest-window
// pull (one page per open), the before-page reroute
// (replay rows buffer instead of being dedup-dropped, then fold through
// applyPulledRows), the per-batch marker timeout (the server deliberately
// omits the marker on partial delivery), and the id-floor background-
// notification gate that replaced the old live/watchdog boolean.
//
// Test seams, and why they are safe:
// - The module under test is rune-bearing, but `deno test` runs it
//   uncompiled. The state machine under test does not depend on reactive
//   proxies (Svelte's compiler owns those in production; svelte-check
//   verifies them), so a passthrough $state shim + an ambient declaration
//   for the type-checker let the classes run as plain fields.
// - The E2EE stack is stubbed at the RelayChannel seam: a fake records
//   history() cursors and replays scripted rows + markers through the same
//   feed() entry the encrypted drain uses. IndexedDB is absent in the test
//   env, so cache reads return [] and writes swallow — the same best-effort
//   paths production uses in private mode.

// Ambient rune types for the whole program (deno test type-checks the
// graph; conversation.svelte.ts itself is normally covered by svelte-check).
declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

// Runtime passthrough: field initializers run as plain assignments.
(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { shouldNotify, WINDOW_PAGE, RelayConversation } =
  await import("./conversation.svelte.ts");
const { unreadStore, tagmaKey } = await import("./unread.svelte.ts");
type ConversationStoreLike = {
  get(id: string): unknown;
};

import type {
  HistoryEntry,
  Participant,
  TagmaReply,
} from "@kallipai/kallipai-common";
import type { Transport } from "./transport.ts";

const AGENT: Participant = { id: "tagma-1", kind: "agent", handle: "Tagma" };
const USER: Participant = { id: "user-1", kind: "human", handle: "alice" };

/** The manage plane's roster answer: the root is the entry nothing created,
 *  with the upstream roll-up beside it (the real body's shape). */
const ROSTER = {
  status: 200,
  body: {
    agents: [
      { id: "child-1", created_by: "root-1" },
      { id: "root-1", created_by: null },
    ],
    upstream_facts: [],
  },
};

/** Let the fire-and-forget cursor send settle. */
const settle = () => new Promise<void>((r) => setTimeout(r, 0));

/** One scripted history pull: the rows the fake replays plus the marker
 * fields (or NO_MARKER to emulate the server's partial-delivery stop). */
interface ScriptedPull {
  rows: { sender: Participant; reply: TagmaReply }[];
  more?: boolean;
  /** The tagma's stamped count and the cursor it was measured at: the marker
   *  carries the pair or nothing, so the badge is assigned, never counted. */
  stamp?: { unread: number; cursor: number };
  noMarker?: boolean;
  after?: number | null;
  before?: number | null;
}

/** A RelayChannel stand-in. history() resolves to a fresh req_id and
 * schedules the scripted rows + marker on a macrotask (the real rows ride
 * the encrypted relay POST — they cannot arrive before the request). Each
 * frame is delivered through the conversation's feed() seam, exactly where
 * the drain would deliver it. */
class FakeChannel {
  readonly conversationId = "conv-1";
  readonly localParticipant = USER;
  script: ScriptedPull[];
  /** Every history() call this channel received, in order. */
  readonly calls: {
    after: number | null;
    before: number | null;
    limit: number | undefined;
  }[] = [];
  /** Deliver the scripted rows synchronously inside history() and only the
   *  marker on a macrotask: pins what the badge reads while a batch is still
   *  arriving (its rows landed, its marker has not). */
  syncRows = false;
  private reqSeq = 0;
  private deliver:
    | ((f: { sender: Participant; reply: TagmaReply }) => void)
    | null = null;

  /** Every manage call this channel received, in order: the cursor write's
   *  route is what these cases assert. */
  readonly manages: { method: string; path: string; body: unknown }[] = [];
  /** Scripted answers, popped per call. A spent script falls back to
   *  {@link manageFallback}. */
  manageScript: { status: number; body?: unknown }[] = [];
  /** The tagma's default answer: the roster GET resolves, writes ack bare. */
  manageFallback(
    method: string,
    path: string,
  ): { status: number; body?: unknown } {
    if (method === "GET" && path === "/agents") return ROSTER;
    return { status: 204 };
  }

  manage(
    method: string,
    path: string,
    body: unknown = null,
  ): Promise<{ status: number; body: unknown }> {
    this.manages.push({ method, path, body });
    const answer =
      this.manageScript.shift() ?? this.manageFallback(method, path);
    return Promise.resolve({
      status: answer.status,
      body: answer.body ?? null,
    });
  }

  constructor(script: ScriptedPull[]) {
    this.script = script;
  }

  history(opts: {
    after?: number | null;
    before?: number | null;
    limit?: number;
  }): Promise<number> {
    const req_id = ++this.reqSeq;
    this.calls.push({
      after: opts.after ?? null,
      before: opts.before ?? null,
      limit: opts.limit,
    });
    const scripted = this.script.shift() ?? { rows: [] };
    scripted.after = opts.after ?? null;
    scripted.before = opts.before ?? null;
    if (this.syncRows) {
      for (const r of scripted.rows) this.deliver?.(r);
      if (!scripted.noMarker) {
        setTimeout(() => this.deliverMarker(scripted, req_id), 0);
      }
    } else {
      setTimeout(() => this.flush(scripted, req_id), 0);
    }
    return Promise.resolve(req_id);
  }

  private flush(s: ScriptedPull, req_id: number): void {
    for (const r of s.rows) this.deliver?.(r);
    if (!s.noMarker) this.deliverMarker(s, req_id);
  }

  private deliverMarker(s: ScriptedPull, req_id: number): void {
    const marker = {
      kind: "history_batch_end" as const,
      req_id,
      count: s.rows.length,
      more: s.more ?? false,
    };
    this.deliver?.({
      sender: AGENT,
      // The stamp is a pair or nothing, like the wire type: no half-stamp.
      reply: s.stamp === undefined ? marker : { ...marker, ...s.stamp },
    });
  }
}

/** The minimal Transport surface the constructor touches. The replies
 * drain never starts in these tests — frames arrive via feed(). */
function fakeTransport(channel: FakeChannel): Transport {
  return {
    localSender: { kind: "user", id: USER.id, handle: USER.handle },
    async *replies() {},
    async *signals() {},
    async *status() {},
    send() {
      return Promise.resolve();
    },
    close() {},
  } as unknown as Transport & { relayChannel: FakeChannel };
  void channel;
}

type RelayConv = InstanceType<typeof RelayConversation>;
const convById = new Map<string, RelayConv>();
const store: ConversationStoreLike = {
  get(id: string) {
    return convById.get(id);
  },
};

function makeConv(channel: FakeChannel): RelayConv {
  const transport = fakeTransport(channel);
  const conv = new RelayConversation(
    channel.conversationId,
    store as never,
    transport,
    "tagma-1",
    "Tagma",
  );
  // pullPage reaches the fake channel through relayTransport.relayChannel;
  // splice it onto the transport stand-in.
  (transport as unknown as { relayChannel: FakeChannel }).relayChannel =
    channel;
  channel["deliver"] = (f) => conv.feed(f.reply, f.sender);
  convById.set(conv.conversationId, conv);
  return conv;
}

/** Record the line-count growth of every transcript assignment: a batched
 *  window grows the transcript once (by the page size), a row-by-row replay
 *  grows it once per row. */
function countTranscriptGrowth(conv: RelayConv): () => number[] {
  const growth: number[] = [];
  let current = conv.transcript;
  Object.defineProperty(conv, "transcript", {
    configurable: true,
    get: () => current,
    set: (v) => {
      const before = current.lines.length;
      if (v.lines.length > before) growth.push(v.lines.length - before);
      current = v;
    },
  });
  return () => growth;
}

function eventRow(id: number, text = `msg ${id}`): HistoryEntry {
  return {
    sender: AGENT,
    reply: {
      kind: "event",
      event: { type: "assistant_content", content: text },
      history_id: id,
      created_at: `2026-08-23T00:00:${String(id % 60).padStart(2, "0")}Z`,
    },
  };
}

function userRow(id: number, text = `note ${id}`): HistoryEntry {
  return {
    sender: USER,
    reply: { kind: "user_message", history_id: id, text },
  };
}

function lineOf(id: number) {
  return {
    historyId: id,
    role: "assistant" as const,
    text: `msg ${id}`,
    sender: { kind: "agent" as const, id: "tagma-1", handle: "Tagma" },
  };
}

Deno.test("catchUp on an empty window takes one recent batch", async () => {
  const channel = new FakeChannel([
    { rows: [userRow(1), eventRow(2)] }, // recent mode: more always false
  ]);
  const conv = makeConv(channel);
  await conv.catchUp();
  assertEquals(conv.transcript.lines.length, 2);
  assertEquals(conv.minRendered, 1);
  assertEquals(conv.maxRendered, 2);
});

Deno.test("a short recent batch disarms the sentinel", async () => {
  const channel = new FakeChannel([{ rows: [eventRow(1)] }]);
  const conv = makeConv(channel);
  await conv.catchUp();
  assertEquals(conv.hasMoreOlder, false);
});

Deno.test(
  "loadOlder reroutes before-page rows and folds them in order",
  async () => {
    // Window holds 60..80; the server's before-page replays 10..59 (below the
    // cursor — the core would dedup-drop them; the reroute must buffer).
    const rows = Array.from({ length: WINDOW_PAGE }, (_, i) =>
      eventRow(10 + i),
    );
    const channel = new FakeChannel([{ rows, more: true }]);
    const conv = makeConv(channel);
    conv.minRendered = 60;
    conv.maxRendered = 80;
    conv.transcript = { lines: [lineOf(60)], status: "idle" };
    await conv.loadOlder();
    assertEquals(conv.transcript.lines[0]!.historyId, 10);
    assertEquals(conv.transcript.lines.length, 1 + WINDOW_PAGE);
    assertEquals(conv.minRendered, 10);
    assertEquals(conv.hasMoreOlder, true); // server said more
  },
);

Deno.test(
  "a timed-out before-page folds buffered rows and stays armed",
  async () => {
    const channel = new FakeChannel([
      { rows: [eventRow(10), eventRow(11)], noMarker: true },
    ]);
    const conv = makeConv(channel);
    conv.pullTimeoutMs = 50;
    conv.minRendered = 60;
    conv.maxRendered = 80;
    await conv.loadOlder();
    assertEquals(conv.transcript.lines.length, 2);
    assertEquals(conv.minRendered, 10);
    assertEquals(conv.hasMoreOlder, true); // timeout proves nothing: retry
  },
);

Deno.test("an old user row never consumes the in-flight echo", () => {
  // c-pin #1: a before-page row whose text matches the in-flight send must
  // not be mis-read as the send's ack (id 42 <= maxRendered 80 reroutes
  // into the page buffer; the echo correlation never sees it).
  const channel = new FakeChannel([]);
  const conv = makeConv(channel);
  conv.minRendered = 60;
  conv.maxRendered = 80;
  (conv as unknown as { pendingInFlight: unknown }).pendingInFlight = {
    localId: -1,
    text: "note 42",
  };
  const pull = { rows: [] as HistoryEntry[], settle: null };
  (conv as unknown as { beforePull: unknown }).beforePull = pull;
  conv.feed(userRow(42, "note 42").reply, USER);
  assertEquals(pull.rows.length, 1); // buffered, not consumed as an ack
  assertEquals(
    (conv as unknown as { pendingInFlight: unknown }).pendingInFlight,
    { localId: -1, text: "note 42" },
  );
});

Deno.test("fresh-device mount race does not permanently disarm", async () => {
  // c-pin #2: readTail empty -> minRendered 0 -> loadOlder must no-op until
  // the window forms (no recent pull at all).
  const channel = new FakeChannel([]);
  const conv = makeConv(channel);
  assertEquals(conv.minRendered, 0);
  await conv.loadOlder();
  assertEquals(channel.script.length, 0); // no request issued
  assertEquals(conv.hasMoreOlder, true);
});

Deno.test("the id floor never notifies for replay frames", () => {
  // c-pin #3: the id floor replaces the live/watchdog gate. Frames at or
  // below the open-time high-water are replay; above it, new.
  assertEquals(shouldNotify(80, eventRow(80).reply), false);
  assertEquals(shouldNotify(80, eventRow(81).reply), true);
  assertEquals(
    shouldNotify(80, { kind: "message_accepted", req_id: 1, queue_depth: 0 }),
    false,
  );
});

Deno.test(
  "catchUp suppresses all replay notifications, then re-arms",
  async () => {
    // Batch rows landing ABOVE a stale floor (a fresh device
    // starts at 0; a partial hydrate leaves the floor below the replay
    // range) would each fire a notification. maybeNotifyBackground needs
    // document/Notification (absent here), so pin the floor itself: MAX for
    // the whole replay, reset to the live edge (maxRendered) after.
    const channel = new FakeChannel([
      { rows: [eventRow(1), eventRow(2), eventRow(3)], more: false },
    ]);
    const conv = makeConv(channel);
    const probe = conv as unknown as { notifyFloor: number };
    const p = conv.catchUp();
    assertEquals(probe.notifyFloor, Number.MAX_SAFE_INTEGER); // replay
    await p;
    assertEquals(probe.notifyFloor, 3); // the live edge after re-arm
    assertEquals(shouldNotify(probe.notifyFloor, eventRow(3).reply), false);
    assertEquals(shouldNotify(probe.notifyFloor, eventRow(4).reply), true);
  },
);

Deno.test("a dead transport folds pulls and drops deliveries", async () => {
  const channel = new FakeChannel([]);
  let enqueues = 0;
  (channel as unknown as { enqueue: () => void }).enqueue = () => {
    enqueues++;
  };
  const conv = makeConv(channel);
  const transport = (
    conv as unknown as {
      transport: { enqueueSignal: () => void };
    }
  ).transport;
  let signals = 0;
  transport.enqueueSignal = () => {
    signals++;
  };
  await conv.run(); // the empty fake replies end the drain: transport=null
  assertEquals(conv.connected, false);

  // A pull on the dead transport folds to a timed-out empty page, and the
  // timed-out page leaves the has-more sentinel armed instead of ending it.
  conv.hasMoreOlder = true;
  const page = await (
    conv as unknown as {
      pullPage(opts: { limit: number }): Promise<{
        rows: unknown[];
        count: number;
        more: boolean;
        timedOut: boolean;
      }>;
    }
  ).pullPage({ limit: 10 });
  assertEquals(page, { rows: [], count: 0, more: true, timedOut: true });
  await (
    conv as unknown as {
      loadOlderPage(k: number): Promise<void>;
    }
  ).loadOlderPage(10);
  assertEquals(conv.hasMoreOlder, true);

  // Deliveries aimed at the dead transport drop without touching it.
  const { ChannelsStore } = await import("./channels.svelte.ts");
  const store = new ChannelsStore();
  const conns = store as unknown as {
    conversations: Map<string, RelayConv>;
  };
  conns.conversations.set("conv-1", conv);
  // Route the index too: without it findByTagma misses and the zero-touch
  // assertion would pass through the index miss, not the connected guard.
  (store as unknown as { tagmaIndex: Map<string, string> }).tagmaIndex.set(
    "tagma-1",
    "conv-1",
  );
  store.deliver({ channel_id: "conv-1" } as Parameters<
    typeof store.deliver
  >[0]);
  store.deliverSignal("tagma-1", { kind: "ping" } as never);
  assertEquals(enqueues, 0);
  assertEquals(signals, 0);
});

Deno.test(
  "catchUp takes one newest window instead of a gap replay",
  async () => {
    // A returning device with rows above its watermark: the window is the unit.
    // A page-by-page gap replay is what dripped the badge and the transcript.
    const channel = new FakeChannel([
      { rows: [eventRow(11), eventRow(12)], more: true },
      { rows: [eventRow(13)], more: true },
    ]);
    const conv = makeConv(channel);
    conv.maxRendered = 10;
    await conv.catchUp();
    assertEquals(channel.calls.length, 1);
    assertEquals(channel.calls[0], {
      after: null,
      before: null,
      limit: WINDOW_PAGE,
    });
    assertEquals(
      conv.transcript.lines.map((l) => l.historyId),
      [11, 12],
    );
    assertEquals(conv.minRendered, 11);
    assertEquals(conv.maxRendered, 12);
  },
);

Deno.test("the adopted window folds in one transcript update", async () => {
  const run = async (ids: number[]) => {
    const channel = new FakeChannel([{ rows: ids.map((id) => eventRow(id)) }]);
    const conv = makeConv(channel);
    const growth = countTranscriptGrowth(conv);
    await conv.catchUp();
    assertEquals(conv.transcript.lines.length, ids.length);
    return growth();
  };
  assertEquals(await run([1, 2, 3]), [3]); // one update for the page
  assertEquals(await run([1]), [1]);
});

Deno.test("the marker's stamp lands once, at the batch's end", async () => {
  unreadStore.reset();
  const channel = new FakeChannel([
    {
      rows: [eventRow(1), eventRow(2), eventRow(3)],
      stamp: { unread: 3, cursor: 3 },
    },
  ]);
  // Rows land inside the request; the marker has not. The badge must not
  // tick while the batch arrives -- the tagma's number rides the marker.
  channel.syncRows = true;
  const conv = makeConv(channel);
  const done = conv.catchUp();
  assertEquals(unreadStore.countOf(tagmaKey("tagma-1")), 0);
  await done;
  assertEquals(unreadStore.countOf(tagmaKey("tagma-1")), 3);
});

Deno.test(
  "each marker's stamp replaces the last, never adds to it",
  async () => {
    unreadStore.reset();
    const channel = new FakeChannel([
      {
        rows: [eventRow(10), eventRow(11), eventRow(12)],
        stamp: { unread: 12, cursor: 12 },
      },
      // The tagma's answer to the open (a cursor write landed): a smaller
      // number at a higher cursor is the newer fact.
      { rows: [eventRow(20)], stamp: { unread: 2, cursor: 40 } },
    ]);
    const conv = makeConv(channel);
    await conv.catchUp();
    assertEquals(unreadStore.countOf(tagmaKey("tagma-1")), 12);
    await conv.catchUp();
    assertEquals(unreadStore.countOf(tagmaKey("tagma-1")), 2);
  },
);

Deno.test("a marker without the stamp leaves the badge untouched", async () => {
  unreadStore.reset();
  unreadStore.setTagmaUnread("tagma-1", { unread: 7, cursor: 3 }); // cold start
  const channel = new FakeChannel([{ rows: [eventRow(1)] }]); // pre-field tagma
  const conv = makeConv(channel);
  await conv.catchUp();
  assertEquals(unreadStore.countOf(tagmaKey("tagma-1")), 7);
});

/** Run the cursor window at once: these cases assert what a landed window
 *  sends. Coalescing has its own test below. */
function landWindowNow(conv: RelayConv): void {
  conv.readCursorTimer = (fn) => {
    fn();
    return 0 as unknown as ReturnType<typeof setTimeout>;
  };
}
Deno.test(
  "mounting the page reports the newest rendered id as read",
  async () => {
    unreadStore.reset();
    const channel = new FakeChannel([{ rows: [eventRow(5), eventRow(6)] }]);
    const conv = makeConv(channel);
    landWindowNow(conv);
    const sent: number[] = [];
    conv.readCursorSender = (cursor) => {
      sent.push(cursor);
      return Promise.resolve();
    };
    await conv.catchUp();
    assertEquals(sent, []); // closed page: a conversation nobody reads
    unreadStore.enter(tagmaKey("tagma-1"));
    conv.markRead();
    assertEquals(sent, [6]);
    conv.markRead(); // monotonic: the same cursor is not re-sent
    assertEquals(sent, [6]);
  },
);

Deno.test("a line landing while viewing reports the cursor; closed, no", () => {
  unreadStore.reset();
  const conv = makeConv(new FakeChannel([]));
  landWindowNow(conv);
  const sent: number[] = [];
  conv.readCursorSender = (cursor) => {
    sent.push(cursor);
    return Promise.resolve();
  };
  // Page closed: the tagma stamps this line itself and the badge arrives by
  // its own carriers, so this client reports nothing.
  conv.feed(
    {
      kind: "event",
      event: { type: "assistant_content", content: "first" },
      history_id: 21,
      created_at: "2026-08-23T00:00:21Z",
    },
    AGENT,
  );
  assertEquals(sent, []);
  unreadStore.enter(tagmaKey("tagma-1"));
  conv.feed(
    {
      kind: "event",
      event: { type: "assistant_content", content: "second" },
      history_id: 22,
      created_at: "2026-08-23T00:00:22Z",
    },
    AGENT,
  );
  assertEquals(sent, [22]);
});

Deno.test("a burst lands one PUT, and the unmount lands the last one", () => {
  unreadStore.reset();
  const conv = makeConv(new FakeChannel([]));
  const sent: number[] = [];
  conv.readCursorSender = (cursor) => {
    sent.push(cursor);
    return Promise.resolve();
  };
  // Hold the window open: nothing is sent until it lands.
  const windows: (() => void)[] = [];
  conv.readCursorTimer = (fn) => {
    windows.push(fn);
    return 0 as unknown as ReturnType<typeof setTimeout>;
  };
  unreadStore.enter(tagmaKey("tagma-1"));
  for (const id of [31, 32, 33]) {
    conv.feed(
      {
        kind: "event",
        event: { type: "assistant_content", content: `line ${id}` },
        history_id: id,
        created_at: "2026-08-23T00:00:31Z",
      },
      AGENT,
    );
  }
  assertEquals(sent, []); // three lines, one window, nothing sent yet
  assertEquals(windows.length, 1); // one window, not one per line
  windows[0]();
  assertEquals(sent, [33]); // the PUT carries the newest cursor
  conv.feed(
    {
      kind: "event",
      event: { type: "assistant_content", content: "line 34" },
      history_id: 34,
      created_at: "2026-08-23T00:00:34Z",
    },
    AGENT,
  );
  assertEquals(sent, [33]); // a fresh window opened
  conv.flushReadCursor(); // the page unmounted
  assertEquals(sent, [33, 34]);
});

Deno.test(
  "the window landing reports the newest rendered id to the tagma",
  async () => {
    unreadStore.reset();
    // The window buffer drops ids it already rendered, so the second pull
    // carries fresh rows (5-7 land closed-page, 8-10 while viewing).
    const channel = new FakeChannel([
      { rows: [eventRow(5), eventRow(6), eventRow(7)] },
      { rows: [eventRow(8), eventRow(9), eventRow(10)] },
    ]);
    const conv = makeConv(channel);
    const sent: number[] = [];
    conv.readCursorSender = (cursor) => {
      sent.push(cursor);
      return Promise.resolve();
    };
    landWindowNow(conv);
    await conv.catchUp();
    assertEquals(sent, []); // closed page: nothing reported
    unreadStore.enter(tagmaKey("tagma-1"));
    await conv.catchUp();
    assertEquals(sent, [10]); // opening the conversation clears it upstream
  },
);

Deno.test(
  "a window adopted over a gap leaves no stale segment rendered",
  async () => {
    const channel = new FakeChannel([
      { rows: [eventRow(500), eventRow(501), eventRow(502)], more: false },
    ]);
    const conv = makeConv(channel);
    conv.transcript = { lines: [100, 101, 102].map(lineOf), status: "idle" };
    conv.minRendered = 100;
    conv.maxRendered = 102;
    await conv.catchUp();
    // Keeping the old cache tail beside the new window would render an unknown
    // gap between them that no page can fill; the window becomes the window.
    assertEquals(
      conv.transcript.lines.map((l) => l.historyId),
      [500, 501, 502],
    );
    assertEquals(conv.minRendered, 500);
    assertEquals(conv.maxRendered, 502);
    // Scroll-up continues from the adopted head (the dropped tail is not a
    // paging cursor any more), and the server page is the contiguous source.
    channel.script.push({ rows: [eventRow(490)], more: true });
    conv.hasMoreOlder = true; // the scripted window is short; force the sentinel
    await conv.loadOlder();
    assertEquals(channel.calls[1]?.before, 500);
    assertEquals(
      conv.transcript.lines.map((l) => l.historyId),
      [490, 500, 501, 502],
    );
  },
);

/** A cached page of durable rows (the shape readTailBefore returns). */
function cachedRows(ids: number[]) {
  return ids.map((id) => ({
    conversationId: "conv-1",
    historyId: id,
    role: "assistant",
    text: `msg ${id}`,
    sender: { kind: "agent" as const, id: "tagma-1", handle: "Tagma" },
  }));
}

/** Replace the leaf's cache read (IndexedDB is absent under deno test). */
function stubCachedPage(conv: RelayConv, rows: unknown[]): void {
  (
    conv as unknown as { readCachedPage: (k: number) => Promise<unknown[]> }
  ).readCachedPage = () => Promise.resolve(rows);
}

Deno.test(
  "a cached page that does not touch the window is skipped for the server page",
  async () => {
    // The cache holds an old segment (100..101) the window (500..502) does not
    // reach; splicing it in would render a hole paging can never fill.
    const channel = new FakeChannel([
      { rows: [eventRow(490), eventRow(491)], more: true },
    ]);
    const conv = makeConv(channel);
    conv.transcript = { lines: [500, 501, 502].map(lineOf), status: "idle" };
    conv.minRendered = 500;
    conv.maxRendered = 502;
    stubCachedPage(conv, cachedRows([100, 101]));
    await conv.loadOlder();
    assertEquals(channel.calls[0], {
      after: null,
      before: 500,
      limit: WINDOW_PAGE,
    });
    assertEquals(
      conv.transcript.lines.map((l) => l.historyId),
      [490, 491, 500, 501, 502],
    );
    assertEquals(conv.minRendered, 490);
  },
);

Deno.test(
  "an adjacent cached page is used and paging continues below it",
  async () => {
    const channel = new FakeChannel([{ rows: [], more: false }]);
    const conv = makeConv(channel);
    conv.transcript = { lines: [500, 501, 502].map(lineOf), status: "idle" };
    conv.minRendered = 500;
    conv.maxRendered = 502;
    // 499 abuts the head: a continuous segment, safe to render.
    stubCachedPage(conv, cachedRows([497, 498, 499]));
    await conv.loadOlder();
    assertEquals(
      conv.transcript.lines.map((l) => l.historyId),
      [497, 498, 499, 500, 501, 502],
    );
    assertEquals(conv.minRendered, 497);
    // The server page continues BELOW the merged rows: re-fetching 497..499
    // would fold to nothing and disarm the sentinel with history still below.
    assertEquals(channel.calls[0]?.before, 497);
  },
);

// -- the production cursor write: one manage PUT on the resolved root --------

Deno.test("the cursor write addresses the root the roster names", async () => {
  unreadStore.reset();
  const channel = new FakeChannel([]);
  const conv = makeConv(channel);
  landWindowNow(conv);
  unreadStore.enter(tagmaKey("tagma-1"));
  conv.maxRendered = 42;
  conv.markRead();
  await settle();
  assertEquals(channel.manages, [
    { method: "GET", path: "/agents", body: null },
    {
      method: "PUT",
      path: "/agents/root-1/external/read-cursor",
      body: { cursor: 42 },
    },
  ]);
  // The ack is a bare 204, so the status class is the whole verdict: the
  // watermark advanced and the same cursor is not sent a second time.
  conv.markRead();
  await settle();
  assertEquals(channel.manages.length, 2);
});

Deno.test("a rejected cursor write keeps its retry point", async () => {
  unreadStore.reset();
  const channel = new FakeChannel([]);
  channel.manageScript = [ROSTER, { status: 500 }];
  const conv = makeConv(channel);
  landWindowNow(conv);
  unreadStore.enter(tagmaKey("tagma-1"));
  conv.maxRendered = 42;
  conv.markRead();
  await settle();
  const puts = () =>
    channel.manages.filter((c) => c.method === "PUT").map((c) => c.body);
  assertEquals(puts(), [{ cursor: 42 }]);
  // A rejected write must not advance the watermark: the next mount reports
  // the same cursor again (the store's cursor is monotonic, so a repeat is
  // harmless), instead of believing a write that never landed.
  conv.markRead();
  await settle();
  assertEquals(puts(), [{ cursor: 42 }, { cursor: 42 }]);
  // The roster is read once: the resolved id is cached on the channel.
  assertEquals(channel.manages.filter((c) => c.method === "GET").length, 1);
});

Deno.test("an unreadable roster sends no cursor write", async () => {
  unreadStore.reset();
  const channel = new FakeChannel([]);
  // The body carries the upstream roll-up but no `agents`: reading the body
  // itself as the roster finds no root, and a write needs one.
  channel.manageScript = [{ status: 200, body: { upstream_facts: [] } }];
  const conv = makeConv(channel);
  landWindowNow(conv);
  unreadStore.enter(tagmaKey("tagma-1"));
  conv.maxRendered = 42;
  conv.markRead();
  await settle();
  assertEquals(
    channel.manages.map((c) => c.method),
    ["GET"],
  );
});

Deno.test("a settled write does not clear a newer one's marker", async () => {
  unreadStore.reset();
  const conv = makeConv(new FakeChannel([]));
  landWindowNow(conv);
  const sent: number[] = [];
  const pending: (() => void)[] = [];
  conv.readCursorSender = (cursor) => {
    sent.push(cursor);
    return new Promise<void>((resolve) => pending.push(resolve));
  };
  unreadStore.enter(tagmaKey("tagma-1"));
  conv.maxRendered = 10;
  conv.markRead();
  assertEquals(sent, [10]);
  conv.maxRendered = 20;
  conv.markRead();
  assertEquals(sent, [10, 20]);
  pending[0](); // the older write lands while the newer is still in flight
  await settle();
  // Re-reporting 20 would be a duplicate PUT: the newer marker still stands.
  conv.markRead();
  assertEquals(sent, [10, 20]);
  pending[1]();
  await settle();
  conv.maxRendered = 21;
  conv.markRead();
  assertEquals(sent, [10, 20, 21]);
});

Deno.test("a rootless install warns once per tunnel", async () => {
  unreadStore.reset();
  const channel = new FakeChannel([]);
  // A roster answer with no `agents` resolves no root, so no cursor can be
  // written. The first report says so; the next window must not repeat it.
  channel.manageScript = [
    { status: 200, body: { upstream_facts: [] } },
    { status: 200, body: { upstream_facts: [] } },
  ];
  const conv = makeConv(channel);
  landWindowNow(conv);
  unreadStore.enter(tagmaKey("tagma-1"));
  const warnings: string[] = [];
  const realWarn = console.warn;
  console.warn = (...args: unknown[]) => {
    warnings.push(args.map((arg) => String(arg)).join(" "));
  };
  try {
    conv.maxRendered = 42;
    conv.markRead();
    await settle();
    conv.maxRendered = 43;
    conv.markRead();
    await settle();
  } finally {
    console.warn = realWarn;
  }
  assertEquals(channel.manages.filter((c) => c.method === "GET").length, 2);
  assertEquals(warnings.filter((w) => w.includes("root agent")).length, 1);
});
