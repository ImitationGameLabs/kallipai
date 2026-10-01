// Tests for the relay leaf's lazy-window machinery: the catch-up pull loop
// (after-pages until the cursor stops advancing), the before-page reroute
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

/** One scripted history pull: the rows the fake replays plus the marker
 * fields (or NO_MARKER to emulate the server's partial-delivery stop). */
interface ScriptedPull {
  rows: { sender: Participant; reply: TagmaReply }[];
  more?: boolean;
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
  private reqSeq = 0;
  private deliver:
    | ((f: { sender: Participant; reply: TagmaReply }) => void)
    | null = null;

  constructor(script: ScriptedPull[]) {
    this.script = script;
  }

  history(opts: {
    after?: number | null;
    before?: number | null;
    limit?: number;
  }): Promise<number> {
    const req_id = ++this.reqSeq;
    const scripted = this.script.shift() ?? { rows: [] };
    scripted.after = opts.after ?? null;
    scripted.before = opts.before ?? null;
    setTimeout(() => this.flush(scripted, req_id), 0);
    return Promise.resolve(req_id);
  }

  private flush(s: ScriptedPull, req_id: number): void {
    for (const r of s.rows) this.deliver?.(r);
    if (!s.noMarker) {
      this.deliver?.({
        sender: AGENT,
        reply: {
          kind: "history_batch_end",
          req_id,
          count: s.rows.length,
          more: s.more ?? false,
        },
      });
    }
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

Deno.test("catchUp pages after-batches until more goes false", async () => {
  const channel = new FakeChannel([
    { rows: [eventRow(11), eventRow(12)], more: true },
    { rows: [eventRow(13)], more: false },
  ]);
  const conv = makeConv(channel);
  conv.maxRendered = 10;
  await conv.catchUp();
  assertEquals(channel.script.length, 0); // script consumed: two pages
  assertEquals(conv.transcript.lines.length, 3);
  assertEquals(conv.maxRendered, 13);
  assertEquals(conv.minRendered, 11);
});

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
