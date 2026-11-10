// The send settles on the POST response alone: the tagma names the row it
// appended, so the line is promoted with no stream frame and no read path at
// all. This is the property that lets a conversation keep writing while its
// SSE is down -- the harness never runs the drain.
//
// Harness: the rune-bearing module runs uncompiled behind a passthrough $state
// shim (the seam the other session tests use); the transport is a structural
// fake that is never iterated, and the sender answers with a fixed id.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { LocalConversation } =
  await import("./conversation/localConversation.svelte.ts");
const { ChannelsStore } = await import("./channels.svelte.ts");

type Transport = import("./transport.ts").Transport;

const transport = {
  localSender: { kind: "user", id: "u-1", handle: "alice" },
  async *replies() {},
  async *signals() {},
  async *status() {},
  close() {},
} as unknown as Transport;

Deno.test(
  "a send settles on the response id with no stream frame",
  async () => {
    const sent: string[] = [];
    const sender = {
      available: true,
      send(text: string) {
        sent.push(text);
        return Promise.resolve({ historyId: 42 });
      },
      close() {},
    };
    const conv = new LocalConversation(
      null as never,
      transport,
      "local",
      sender as never,
    );
    conv.send("hello");
    // One microtask turn: the POST resolves, the pump settles the line.
    await new Promise((resolve) => setTimeout(resolve, 0));

    assertEquals(sent, ["hello"]);
    assertEquals(conv.outbox.pending.length, 0, "the queue is drained");
    assertEquals(conv.outbox.pendingInFlight, null, "the pump slot is free");
    const lines = conv.transcript.rows;
    assertEquals(lines.length, 1, "one line, not a second bubble");
    assertEquals(
      lines[0]!.historyId,
      42,
      "the row id replaces the synthetic one",
    );
    assertEquals(lines[0]!.status, "sent", "the line is confirmed");
    assertEquals(
      conv.transcript.maxRowId,
      0,
      "the cursor stays with the read path, not the response id",
    );
  },
);

Deno.test("a response without an id leaves the line to its echo", async () => {
  const sender = {
    available: true,
    send() {
      return Promise.resolve({ historyId: null });
    },
    close() {},
  };
  const conv = new LocalConversation(
    null as never,
    transport,
    "local",
    sender as never,
  );
  conv.send("hello");
  await new Promise((resolve) => setTimeout(resolve, 0));

  const lines = conv.transcript.rows;
  assertEquals(lines.length, 1);
  assertEquals(lines[0]!.historyId! < 0, true, "the synthetic id stays");
  assertEquals(lines[0]!.status, "sending", "settlement still rides the echo");
  assertEquals(
    conv.outbox.pendingInFlight?.text,
    "hello",
    "the in-flight slot waits for the frame",
  );
});

/** A transport whose drain ends at once: the stream is dead on arrival. */
function deadStream(): Transport {
  return {
    localSender: { kind: "user", id: "u-1", handle: "alice" },
    async *replies() {},
    async *signals() {},
    async *status() {},
    close() {},
  } as unknown as Transport;
}

/** A transport whose drain never ends: the rebuilt stream stays open. */
function openStream(until: Promise<void>): Transport {
  return {
    localSender: { kind: "user", id: "u-1", handle: "alice" },
    // deno-lint-ignore require-yield -- no frame: the gate is the close.
    async *replies() {
      await until;
    },
    // deno-lint-ignore require-yield -- no frame: the gate is the close.
    async *signals() {
      await until;
    },
    // deno-lint-ignore require-yield -- no frame: the gate is the close.
    async *status() {
      await until;
    },
    close() {},
  } as unknown as Transport;
}

Deno.test("a dead read path keeps the queue and its writer", async () => {
  const convs = new Map<string, unknown>();
  const store = { get: (id: string) => convs.get(id) } as never;

  const sent: string[] = [];
  let release: (r: { historyId: number }) => void = () => {};
  const held = new Promise<{ historyId: number }>((resolve) => {
    release = resolve;
  });
  const sender = {
    available: true,
    send(text: string) {
      sent.push(text);
      return held;
    },
    close() {},
  };

  let openTheNewStream: () => void = () => {};
  const openGate = new Promise<void>((resolve) => {
    openTheNewStream = resolve;
  });
  let rebuilt = 0;
  const conv = new LocalConversation(
    store,
    deadStream(),
    "local",
    sender as never,
    () => {
      rebuilt += 1;
      return openStream(openGate);
    },
  );
  convs.set("local", conv);
  // The rebuild runs on a ladder short enough to spend: the wait itself is the
  // production one.
  conv.link.backoff = { min: 1, max: 1 };
  assertEquals(conv.link.connected, true, "the stream is bound");

  // abandonPending() is the relay leaf's terminal cleanup (the direct leaf has
  // no restart-free path left to reach it): the queue surviving the drain is
  // the observable that says nothing tore it down.

  const drain = conv.run();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assertEquals(
    conv.link.status,
    "reconnecting",
    "the stream ended, the session did not",
  );
  assertEquals(
    conv.link.connected,
    false,
    "no connected signal while the stream is being rebuilt",
  );

  conv.send("still typing");
  await new Promise((resolve) => setTimeout(resolve, 0));
  assertEquals(sent, ["still typing"], "the write carrier is the POST");
  assertEquals(conv.transcript.rows[0]!.status, "sending", "held, not failed");
  assertEquals(
    conv.outbox.pendingInFlight?.text,
    "still typing",
    "the writer holds the line across the read path's death",
  );

  // The stream returns and is rebuilt; the held POST then completes.
  await new Promise((resolve) => setTimeout(resolve, 10));
  assertEquals(rebuilt, 1, "a fresh stream was built under the same sender");
  conv.onTransportState("resumed");
  release({ historyId: 7 });
  await new Promise((resolve) => setTimeout(resolve, 10));

  assertEquals(conv.outbox.pending.length, 0);
  assertEquals(conv.outbox.pendingInFlight, null);
  assertEquals(conv.transcript.rows[0]!.historyId, 7);
  assertEquals(conv.transcript.rows[0]!.status, "sent");
  assertEquals(conv.link.status, "open");
  assertEquals(conv.link.connected, true, "the rebuilt stream clears it");

  conv.close();
  openTheNewStream();
  await drain;
});

/** One microtask turn: the POST resolves and the pump settles the line. */
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

/** Feed one authored frame through the shared core, as the drain would. */
function feedFrame(conv: unknown, historyId: number, text = "hello"): void {
  (
    conv as { applyReplyCore(reply: unknown, sender: unknown): void }
  ).applyReplyCore(
    {
      kind: "user_message",
      history_id: historyId,
      text,
      created_at: "2026-11-06T00:00:00Z",
    },
    undefined,
  );
}

/** A conversation wired into a store, so the shared core sees it as live. */
function liveConversation(sender: unknown) {
  const convs = new Map<string, unknown>();
  const store = { get: (id: string) => convs.get(id) } as never;
  const conv = new LocalConversation(
    store,
    transport,
    "local",
    sender as never,
  );
  convs.set("local", conv);
  return conv;
}

Deno.test(
  "the echo of a settled row is a replay, not a second bubble",
  async () => {
    const sender = {
      available: true,
      send() {
        return Promise.resolve({ historyId: 42 });
      },
      close() {},
    };
    const conv = liveConversation(sender);
    conv.send("hello");
    await tick();

    feedFrame(conv, 42);
    const lines = conv.transcript.rows;
    assertEquals(lines.length, 1, "the echo renders nothing new");
    assertEquals(lines[0]!.historyId, 42);
    assertEquals(lines[0]!.status, "sent");
    assertEquals(
      conv.transcript.maxRowId,
      0,
      "a replay moves no cursor: it proves only that this row is here",
    );
  },
);

Deno.test("a row below a settled id lands and one above advances", async () => {
  const sender = {
    available: true,
    send() {
      return Promise.resolve({ historyId: 43 });
    },
    close() {},
  };
  const conv = liveConversation(sender);
  conv.send("hello");
  await tick();

  feedFrame(conv, 42);
  assertEquals(
    conv.transcript.rows.length,
    2,
    "the lower row is not swallowed by the settled id",
  );
  assertEquals(conv.transcript.maxRowId, 42);

  feedFrame(conv, 44);
  assertEquals(conv.transcript.rows.length, 3);
  assertEquals(conv.transcript.maxRowId, 44, "new rows move the cursor");
});

Deno.test("a replay cannot pull the cursor over a hole below it", async () => {
  const pulls: unknown[] = [];
  const sender = {
    available: true,
    send() {
      return Promise.resolve({ historyId: 10 });
    },
    pullHistory(opts: unknown) {
      pulls.push(opts);
      return Promise.resolve({ rows: [], more: false, undecodable: 0 });
    },
    close() {},
  };
  const conv = liveConversation(sender);
  // The read path is down: the POST settles row 10 with rows 6..9 never
  // rendered. The echo of row 10 is a replay, and it must not move the
  // cursor -- doing so would make the backfill skip 6..9 for good.
  conv.send("hello");
  await tick();
  assertEquals(conv.transcript.maxRowId, 0, "the settled row moves no cursor");
  feedFrame(conv, 10);
  assertEquals(conv.transcript.rows.length, 1, "the replay renders nothing");
  assertEquals(
    conv.transcript.maxRowId,
    0,
    "the replay moves no cursor either",
  );
  await conv.catchUp();
  assertEquals(pulls.length, 1);
  const after = (pulls[0] as { after?: number | null }).after;
  assertEquals(after ?? 0, 0, "the backfill starts below the hole, not at 10");
});

Deno.test("the channel lamp goes dark while the stream rebuilds", () => {
  const sender = {
    available: true,
    send() {
      return Promise.resolve({ historyId: 7 });
    },
    close() {},
  };
  const conv = liveConversation(sender);
  assertEquals(conv.link.connected, true, "attached with a healthy stream");
  conv.onTransportState("reconnecting");
  assertEquals(
    conv.link.connected,
    false,
    "a transport retry darkens the lamp",
  );
  conv.onTransportState("resumed");
  assertEquals(conv.link.connected, true, "the lamp returns with the stream");
});

Deno.test("the offline setup question outlives a dark lamp", () => {
  const store = new ChannelsStore();
  const conv = new LocalConversation(store as never, transport, "local", {
    available: true,
    send: () => Promise.resolve({ historyId: 1 }),
    close() {},
  } as never);
  (
    store as unknown as { conversations: Map<string, unknown> }
  ).conversations.set("local", conv);
  conv.onTransportState("reconnecting");
  assertEquals(store.localAttached, true, "a rebuild does not unbind it");
  assertEquals(store.localConnected, false, "while the lamp is dark");
});

Deno.test("a line typed during a rebuild goes out", async () => {
  const sent: string[] = [];
  const sender = {
    available: true,
    send(text: string) {
      sent.push(text);
      return Promise.resolve({ historyId: 9 });
    },
    close() {},
  };
  const store = new ChannelsStore();
  const conv = new LocalConversation(
    store as never,
    transport,
    "local",
    sender as never,
  );
  (
    store as unknown as { conversations: Map<string, unknown> }
  ).conversations.set("local", conv);
  conv.onTransportState("reconnecting");
  assertEquals(conv.link.connected, false, "the channel lamp is dark");
  assertEquals(conv.link.canWrite, true, "the write axis ignores the rebuild");

  // The page's own path: the store hands it to the conversation.
  store.send("local", "still typing");
  await tick();
  assertEquals(sent, ["still typing"], "the post leaves mid-rebuild");
  assertEquals(conv.transcript.rows.length, 1, "the line is rendered");
  assertEquals(conv.transcript.rows[0]!.historyId, 9, "the row landed");
  assertEquals(conv.transcript.rows[0]!.status, "sent", "never failed");

  conv.onTransportState("resumed");
  await tick();
  assertEquals(conv.link.connected, true, "the lamp comes back");
  assertEquals(conv.transcript.rows[0]!.historyId, 9);
  assertEquals(conv.transcript.rows[0]!.status, "sent");
  assertEquals(conv.outbox.pending.length, 0, "nothing is left behind");
});

Deno.test("a dead carrier refuses the line and says so", () => {
  const sender = {
    available: true,
    send() {
      return Promise.resolve({ historyId: 1 });
    },
    close() {},
  };
  const store = new ChannelsStore();
  const conv = new LocalConversation(
    store as never,
    transport,
    "local",
    sender as never,
  );
  (
    store as unknown as { conversations: Map<string, unknown> }
  ).conversations.set("local", conv);
  sender.available = false; // the carrier closed under the conversation
  assertEquals(conv.link.canWrite, false);
  store.send("local", "hello");
  assertEquals(conv.transcript.rows.length, 0, "nothing lands in a dead chat");
  assertEquals(
    conv.link.error instanceof Error,
    true,
    "the refusal is observable, not silent",
  );
});

Deno.test("a zero id is not a row and never settles", async () => {
  const sender = {
    available: true,
    send() {
      return Promise.resolve({ historyId: 0 });
    },
    close() {},
  };
  const conv = liveConversation(sender);
  conv.send("hello");
  await tick();

  const lines = conv.transcript.rows;
  assertEquals(lines.length, 1);
  assertEquals(lines[0]!.status, "sending", "a zero is not a row");
  assertEquals(
    conv.outbox.pendingInFlight?.text,
    "hello",
    "the echo still owns it",
  );
});

Deno.test("an echo never claims the next identical send", async () => {
  const convs = new Map<string, unknown>();
  const store = { get: (id: string) => convs.get(id) } as never;
  const pending: Array<(id: number) => void> = [];
  const sender = {
    available: true,
    send() {
      return new Promise((resolve) => {
        pending.push((id: number) => resolve({ historyId: id }));
      });
    },
    close() {},
  };
  const conv = new LocalConversation(
    store,
    transport,
    "local",
    sender as never,
  );
  convs.set("local", conv);

  conv.send("ok");
  conv.send("ok");
  await tick();
  assertEquals(pending.length, 1, "one POST at a time");

  pending[0]!(10);
  await tick();
  assertEquals(pending.length, 2, "the second POST is out");

  // The first send's echo arrives while the second is in flight.
  feedFrame(conv, 10, "ok");
  assertEquals(
    conv.outbox.pendingInFlight?.text,
    "ok",
    "the replay leaves the second line's slot alone",
  );

  pending[1]!(11);
  await tick();
  const lines = conv.transcript.rows;
  assertEquals(lines.length, 2, "one bubble per send");
  assertEquals(
    lines.map((l) => l.historyId),
    [10, 11],
  );
  assertEquals(
    lines.map((l) => l.status),
    ["sent", "sent"],
  );
});
