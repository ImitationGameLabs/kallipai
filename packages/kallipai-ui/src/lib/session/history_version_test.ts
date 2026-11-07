// The history numbering contract: a page says which numbering it is written
// under, a build refuses one it does not read, and the window freezes rather
// than placing rows from another log. Freezing is not "stop pulling": live
// frames keep arriving, so the ingest path must also stop stamping positions,
// moving the cursor, and writing cache rows.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals, assertRejects } = await import("@std/assert");
const { LocalConversation } = await import("./conversation.svelte.ts");
const { DirectMessageSender, HistorySchemaMismatch } =
  await import("./messageSender.svelte.ts");
import { HISTORY_SCHEMA_VERSION } from "@kallipai/kallipai-client";
import { DirectTransport } from "./directTransport.ts";
import { LOCAL_OPERATOR_SENDER } from "../transcript.ts";

type PageRequest = {
  afterSeq?: number | null;
  beforeSeq?: number | null;
  limit?: number;
};

type Page = {
  rows: ReturnType<typeof row>[];
  more: boolean;
  undecodable?: number;
  next?: number | null;
  schema_version?: number;
};

/** One inbound row as the history face hands it back. */
function row(historyId: number, seq = historyId) {
  return {
    sender: { id: "u1", kind: "human", handle: "Alice" },
    reply: {
      kind: "user_message",
      history_id: historyId,
      seq,
      text: `row ${historyId}`,
      created_at: "2026-10-22T00:00:00Z",
    },
  };
}

function page(rows: ReturnType<typeof row>[], more: boolean): Page {
  return {
    rows,
    more,
    next: rows.length > 0 ? rows[rows.length - 1]!.reply.seq : null,
  };
}

/** A page written under a numbering this build does not read. */
function foreignPage(): Page {
  return { rows: [], more: false, next: null, schema_version: 99 };
}

/** A local conversation whose history face is scripted by the pull request. */
function conversation(script: (opts: PageRequest) => Page) {
  const seen: PageRequest[] = [];
  const client = {
    externalEventStream: () => {
      throw new Error("these cases never open the stream");
    },
    externalHistory: (_id: string, opts: PageRequest) => {
      seen.push(opts);
      return Promise.resolve(script(opts));
    },
  };
  const transport = new DirectTransport(
    client as never,
    "root",
    LOCAL_OPERATOR_SENDER,
  );
  const conv = new LocalConversation(
    { get: () => undefined } as never,
    transport,
    "conv-local",
    new DirectMessageSender(client as never, "root"),
  );
  return { conv, seen, client };
}

/** Deliver one live content frame the way the drain would. */
function feed(conv: unknown, reply: Record<string, unknown>): void {
  const core = conv as { applyReplyCore(r: unknown, s: unknown): void };
  core.applyReplyCore(reply, { id: "a1", kind: "agent", handle: "Root" });
}

function frame(historyId: number, seq?: number, text = `line ${historyId}`) {
  return {
    kind: "event",
    event: { type: "assistant_content", content: text },
    history_id: historyId,
    ...(seq === undefined ? {} : { seq }),
    created_at: "2026-10-22T00:00:00Z",
  };
}

/** The fields the freeze is about, reached through the protected seam. */
type Core = {
  historyError: string | null;
  historyFrozen: boolean;
  lastSeq: number;
  notePullFailure(e: unknown): void;
  applyPulledRows(rows: unknown[]): number;
};

Deno.test(
  "a page that names another numbering is refused at the pull",
  async () => {
    const far = new DirectMessageSender(
      { externalHistory: () => Promise.resolve(foreignPage()) } as never,
      "root",
    );
    await assertRejects(
      () => far.pullHistory({ limit: 1 }),
      HistorySchemaMismatch,
      "v99",
    );
  },
);

Deno.test(
  "a page that names no numbering is read as this build's",
  async () => {
    const silent = new DirectMessageSender(
      {
        externalHistory: () =>
          Promise.resolve({
            ...page([row(1)], false),
            schema_version: undefined,
          }),
      } as never,
      "root",
    );
    const answer = await silent.pullHistory({ limit: 1 });
    assertEquals(answer.rows.length, 1);
  },
);

Deno.test(
  "a page that names this build's numbering passes the pull",
  async () => {
    const same = new DirectMessageSender(
      {
        externalHistory: () =>
          Promise.resolve({
            ...page([row(1)], false),
            schema_version: HISTORY_SCHEMA_VERSION,
          }),
      } as never,
      "root",
    );
    const answer = await same.pullHistory({ limit: 1 });
    assertEquals(answer.rows.length, 1);
  },
);

Deno.test("a page from another numbering freezes the window", async () => {
  const { conv, seen } = conversation(() => foreignPage());
  conv.lastSeq = 10;
  await conv.catchUp(3);
  const core = conv as unknown as Core;
  assertEquals(core.historyError, "protocol");
  assertEquals(conv.lastSeq, 10, "no position moves");
  assertEquals(conv.transcript.lines.length, 0, "no rows are placed");
  const asked = seen.length;
  assertEquals(asked, 1);

  await conv.catchUp(3);
  conv.minRowId = 5;
  await conv.loadOlder(3);
  assertEquals(seen.length, asked, "a frozen window asks nothing more");
});

Deno.test(
  "a frozen window renders a live frame without placing it",
  async () => {
    const { conv } = conversation(() => foreignPage());
    conv.lastSeq = 10;
    await conv.catchUp(3);
    const core = conv as unknown as Core;
    assertEquals(core.historyFrozen, true);

    feed(conv, frame(99, 11, "after the freeze"));
    // A position below the stale cursor is not a replay here: the cursor is in
    // another numbering, so the frame renders and only row ids dedup.
    feed(conv, frame(98, 5, "older than the stale cursor"));

    assertEquals(
      conv.transcript.lines.map((l) => l.text),
      ["after the freeze", "older than the stale cursor"],
    );
    assertEquals(
      conv.lastSeq,
      10,
      "the cursor stays where the freeze found it",
    );
    assertEquals(
      conv.transcript.lines.map((l) => l.seq),
      [undefined, undefined],
      "a frozen window stamps no position",
    );
  },
);

Deno.test("a freeze is not downgraded by a later failure", () => {
  const { conv } = conversation(() => page([], false));
  const core = conv as unknown as Core;
  core.notePullFailure(new HistorySchemaMismatch(99, HISTORY_SCHEMA_VERSION));
  assertEquals(core.historyError, "protocol");
  core.notePullFailure(new Error("a plain failure"));
  assertEquals(
    core.historyError,
    "protocol",
    "the mismatch stands until a reload",
  );
});

Deno.test(
  "a page landing after the freeze renders its text and moves no cursor",
  () => {
    const { conv } = conversation(() => page([], false));
    const core = conv as unknown as Core;
    core.notePullFailure(new HistorySchemaMismatch(99, HISTORY_SCHEMA_VERSION));
    const added = core.applyPulledRows([row(7, 1), row(8, 2)]);
    assertEquals(added, 2, "the page's text lands");
    // The rows carry the page's own positions, which is why they are text and
    // nothing more here: the cursor they would advance lives in another
    // numbering, so it stays where the freeze found it.
    assertEquals(conv.lastSeq, 0, "no position moves");
  },
);
Deno.test("a frozen window adopts no cached positions", () => {
  const { conv } = conversation(() => page([], false));
  const core = conv as unknown as Core;
  core.notePullFailure(new HistorySchemaMismatch(99, HISTORY_SCHEMA_VERSION));
  conv.lastSeq = 10;
  conv.seedRenderedPositions([
    { historyId: 7, role: "assistant", text: "a", seq: 7 },
    { historyId: 8, role: "assistant", text: "b", seq: 8 },
  ]);
  assertEquals(conv.lastSeq, 10, "the cursor stays where the freeze found it");
});

Deno.test("the numbering this client reads is pinned to the literal", () => {
  // Deliberately the literal, not a comparison against the constant: the
  // Rust half asserts the same 1 in
  // crates/kallipai-tagma/src/routes/message/tests.rs, and a pair that drifts
  // apart does not degrade -- every reader freezes.
  assertEquals(HISTORY_SCHEMA_VERSION, 1);
});
