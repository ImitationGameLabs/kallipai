// The direct leaf's resume: the position cursor, the single-flight repair, and
// what moves the cursor (a rendered row) versus what must not (a page's `next`,
// an unstamped frame, a frame at or below the cursor).
//
// The module under test is rune-bearing and `deno test` runs it uncompiled, so
// a passthrough `$state` shim lets it run with plain fields.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { LocalConversation } = await import("./conversation.svelte.ts");
import { DirectTransport } from "./directTransport.ts";
import { DirectMessageSender } from "./messageSender.svelte.ts";
import {
  EMPTY_TRANSCRIPT,
  LOCAL_OPERATOR_SENDER,
  replaceLineId,
  withUserLine,
} from "../transcript.ts";

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
};

/** One inbound row as the history face hands it back: a row id beside the
 *  position the cursor lives in. */
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

/** A local conversation over a transport whose history face is scripted by
 *  the pull request it receives. */
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
  return { conv, seen, transport };
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

Deno.test(
  "a repair pulls forward until the tagma says there is no more",
  async () => {
    const { conv, seen } = conversation((opts) =>
      opts.afterSeq === 10
        ? page([row(11), row(12)], true)
        : page([row(13)], false),
    );
    conv.lastSeq = 10;
    await conv.catchUp(3);
    assertEquals(seen, [
      { afterSeq: 10, limit: 3 },
      { afterSeq: 12, limit: 3 },
    ]);
    // The cursor lands on the newest rendered position, not on the page's edge.
    assertEquals(conv.lastSeq, 13);
  },
);

Deno.test("a seq jump starts one repair, not one per frame", async () => {
  const { conv, seen } = conversation((opts) =>
    opts.afterSeq === 10
      ? page([row(11), row(12), row(15)], false)
      : page([], false),
  );
  conv.lastSeq = 10;
  // Two frames above the hole: the first starts the repair, the second asks
  // the running pass for one more sweep instead of starting its own.
  feed(conv, frame(15, 15));
  feed(conv, frame(16, 16));
  await conv.catchUp(3);
  // Both frames ask for a repair; the pass begins at the cursor either way.
  assertEquals(seen[0]?.afterSeq, 10);
  assertEquals(
    seen.filter((o) => o.afterSeq === 10).length,
    1,
    "one repair begins at the cursor, however many frames jump",
  );
});

Deno.test("a frame at or below the cursor is dropped", () => {
  const { conv } = conversation(() => page([], false));
  conv.lastSeq = 10;
  feed(conv, frame(9, 10));
  assertEquals(
    conv.transcript.lines.length,
    0,
    "a covered position renders nothing",
  );
  assertEquals(conv.lastSeq, 10);
});

Deno.test("an unstamped frame renders without moving the cursor", () => {
  const { conv } = conversation(() => page([], false));
  conv.lastSeq = 10;
  feed(conv, frame(11));
  assertEquals(conv.transcript.lines.length, 1, "the row still renders");
  assertEquals(conv.lastSeq, 10, "an unknown position moves nothing");
});

Deno.test(
  "a retention hole moves the cursor past it, and the warning names it",
  async () => {
    const { conv } = conversation(() => page([row(20), row(21)], false));
    conv.lastSeq = 10;
    const warns: string[] = [];
    const realWarn = console.warn;
    console.warn = (...args: unknown[]) => {
      warns.push(args.map((a) => String(a)).join(" "));
    };
    try {
      await conv.catchUp(3);
    } finally {
      console.warn = realWarn;
    }
    assertEquals(conv.lastSeq, 21);
    assertEquals(warns.length, 1, "the hole is named once");
  },
);

Deno.test("a warm cache resumes from its highest position", async () => {
  // An empty page at the front means one of two things, so it is probed:
  // the script answers "still the live tail" at this position.
  const { conv, seen } = conversation((opts) =>
    opts.limit === 1
      ? { rows: [row(9)], more: false, next: 9 }
      : page([], false),
  );
  conv.seedRenderedPositions([
    { historyId: 7, role: "assistant", text: "a", seq: 7 },
    { historyId: 8, role: "assistant", text: "b", seq: 8 },
    { historyId: 9, role: "assistant", text: "c", seq: 9 },
  ]);
  assertEquals(conv.lastSeq, 9);
  await conv.catchUp(3);
  assertEquals(seen, [{ afterSeq: 9, limit: 3 }, { limit: 1 }]);
});

Deno.test(
  "a cursor the newest page no longer reaches resets the window",
  async () => {
    const { conv, seen } = conversation((opts) => {
      if (opts.afterSeq === 900) return { rows: [], more: false, next: null };
      if (opts.limit === 1) return { rows: [row(30)], more: false, next: 30 };
      return page([row(28), row(29), row(30)], false);
    });
    conv.seedRenderedPositions([
      { historyId: 898, role: "assistant", text: "a", seq: 898 },
      { historyId: 899, role: "assistant", text: "b", seq: 899 },
      { historyId: 900, role: "assistant", text: "c", seq: 900 },
    ]);

    await conv.catchUp(3);

    assertEquals(seen, [
      { afterSeq: 900, limit: 3 },
      { limit: 1 },
      { limit: 3 },
    ]);
    // The rows and the cursor move together: old-numbered lines gone, the
    // window re-opened from the newest page.
    assertEquals(conv.lastSeq, 30);
    assertEquals(
      conv.transcript.lines.map((l) => l.historyId),
      [28, 29, 30],
    );
  },
);

Deno.test(
  "a sweep at the tail probes again, and a renumbered log resets it",
  async () => {
    let renumbered = false;
    const { conv, seen } = conversation((opts) => {
      if (opts.limit === 1) {
        return renumbered
          ? { rows: [row(30)], more: false, next: 30 }
          : { rows: [row(900)], more: false, next: 900 };
      }
      if (opts.afterSeq != null) return { rows: [], more: false, next: null };
      return page([row(28), row(29), row(30)], false);
    });
    conv.lastSeq = 900;

    // The tail leaves the cursor alone; the next sweep asks again, so a store
    // rebuilt under new ids is not hidden behind an answer that was true once.
    await conv.catchUp(3);
    assertEquals(seen.filter((o) => o.limit === 1).length, 1);
    assertEquals(conv.lastSeq, 900);

    renumbered = true;
    await conv.catchUp(3);
    assertEquals(seen.filter((o) => o.limit === 1).length, 2);
    assertEquals(conv.lastSeq, 30);
    assertEquals(
      conv.transcript.lines.map((l) => l.historyId),
      [28, 29, 30],
    );
  },
);

Deno.test("a log that holds no history at all resets the window", async () => {
  let emptied = false;
  const { conv, seen } = conversation((opts) => {
    if (opts.limit === 1 || opts.afterSeq != null || emptied) {
      return { rows: [], more: false, next: null };
    }
    return page([row(28), row(29)], false);
  });
  // A window from a store that exists, holding its position.
  await conv.catchUp(3);
  assertEquals(
    conv.transcript.lines.map((l) => l.historyId),
    [28, 29],
  );
  assertEquals(conv.lastSeq, 29);

  // The store is gone: an empty sweep, a probe that finds nothing, and rows
  // on screen that belong to no log. The position goes with them.
  emptied = true;
  conv.lastSeq = 900;
  await conv.catchUp(3);

  assertEquals(seen.filter((o) => o.limit === 1).length, 1);
  assertEquals(conv.transcript.lines.length, 0);
  assertEquals(conv.lastSeq, 0);
});

Deno.test("a reset keeps the line the pump holds in flight", async () => {
  const { conv } = conversation((opts) => {
    if (opts.limit === 1) return { rows: [row(30)], more: false, next: 30 };
    if (opts.afterSeq != null) return { rows: [], more: false, next: null };
    return page([row(28), row(29), row(30)], false);
  });
  conv.lastSeq = 900;
  // The pump shifted this line out of `pending` and its POST is still out,
  // so a reset must keep it: nothing else holds the text.
  conv.transcript = withUserLine(
    EMPTY_TRANSCRIPT,
    "hello",
    -7,
    LOCAL_OPERATOR_SENDER,
  );
  conv.pendingInFlight = { localId: -7, text: "hello" };

  await conv.catchUp(3);

  assertEquals(
    conv.transcript.lines.map((l) => l.historyId).sort((a, b) => a - b),
    [-7, 28, 29, 30],
  );
  assertEquals(conv.lastSeq, 30);
});

Deno.test("a missed frame repairs from the local cursor", async () => {
  const { conv, seen } = conversation((opts) =>
    opts.limit === 1
      ? { rows: [row(10)], more: false, next: 10 }
      : page([], false),
  );
  conv.lastSeq = 10;
  const wire = (
    conv as unknown as {
      transport: { onMissed: (() => void) | null };
    }
  ).transport;
  wire.onMissed?.();
  await conv.catchUp(3);
  assertEquals(seen.length, 2);
  assertEquals(seen[0]?.afterSeq, 10, "the marker resumes from the cursor");
  assertEquals(seen[1], { limit: 1 }, "an empty page is probed once");
});

Deno.test(
  "a cold start opens the newest window and reports its position",
  async () => {
    const { conv, seen } = conversation(() => page([row(30), row(31)], false));
    await conv.catchUp(3);
    assertEquals(
      seen,
      [{ limit: 3 }],
      "no cursor: the newest window, not from zero",
    );
    assertEquals(conv.lastSeq, 31);
  },
);

Deno.test("an echo hands the promoted line its position", () => {
  const { conv, seen } = conversation(() => page([], false));
  conv.lastSeq = 10;
  conv.transcript = withUserLine(
    EMPTY_TRANSCRIPT,
    "hello",
    -7,
    LOCAL_OPERATOR_SENDER,
  );
  conv.pendingInFlight = { localId: -7, text: "hello" };
  feed(conv, {
    kind: "user_message",
    history_id: 41,
    seq: 11,
    text: "hello",
  });
  assertEquals(conv.lastSeq, 11, "the row's position moves the cursor");
  assertEquals(seen.length, 0, "the row was on screen: nothing to pull");
  assertEquals(conv.transcript.lines[0]!.seq, 11, "the line carries it");
});

Deno.test("an echo after the ack still adopts the position", () => {
  const { conv, seen } = conversation(() => page([], false));
  conv.lastSeq = 10;
  // The POST response settled the line under its real id first.
  conv.transcript = replaceLineId(
    withUserLine(EMPTY_TRANSCRIPT, "hello", -7, LOCAL_OPERATOR_SENDER),
    -7,
    41,
  );
  feed(conv, { kind: "user_message", history_id: 41, seq: 11, text: "hello" });
  assertEquals(conv.transcript.lines[0]!.seq, 11, "the settled line takes it");
  assertEquals(conv.lastSeq, 11);
  assertEquals(seen.length, 0);
});

Deno.test("a page read through the anchor still lands its rows", async () => {
  const { conv } = conversation((opts) =>
    opts.afterSeq === 10
      ? page([row(11), row(12)], true)
      : page([row(13), row(14)], false),
  );
  conv.lastSeq = 10;
  await conv.catchUp(4);
  assertEquals(
    conv.transcript.lines.map((l) => l.historyId),
    [11, 12, 13, 14],
    "every row of every page is on screen",
  );
  assertEquals(conv.lastSeq, 14);
});

Deno.test(
  "a page that renders nothing steps the cursor over the block",
  async () => {
    const { conv, seen } = conversation((opts) =>
      opts.afterSeq === 10
        ? { rows: [], more: true, undecodable: 3, next: 13 }
        : page([row(14)], false),
    );
    conv.lastSeq = 10;
    await conv.catchUp(3);
    assertEquals(
      seen,
      [
        { afterSeq: 10, limit: 3 },
        { afterSeq: 13, limit: 3 },
      ],
      "the next read starts at the page edge, not at the cursor",
    );
    assertEquals(
      conv.lastSeq,
      14,
      "the cursor steps over the block onto the next row",
    );
  },
);
Deno.test(
  "a dead tail is stepped over even when nothing lies above it",
  async () => {
    const { conv, seen } = conversation(() => ({
      rows: [row(11)],
      more: false,
      undecodable: 1,
      next: 13,
    }));
    conv.lastSeq = 10;
    const warns: string[] = [];
    const realWarn = console.warn;
    console.warn = (...args: unknown[]) => {
      warns.push(args.map((a) => String(a)).join(" "));
    };
    try {
      await conv.catchUp(3);
    } finally {
      console.warn = realWarn;
    }
    assertEquals(seen, [{ afterSeq: 10, limit: 3 }], "the tail asks once");
    assertEquals(conv.lastSeq, 13, "the cursor lands on the tail's edge");
    assertEquals(warns.length, 1, "the block is named once");
  },
);

Deno.test(
  "a dead tail with nothing decoded is stepped over, not probed",
  async () => {
    const { conv, seen } = conversation((opts) =>
      opts.afterSeq == null
        ? page([], false)
        : { rows: [], more: false, undecodable: 3, next: opts.afterSeq + 3 },
    );
    conv.lastSeq = 10;

    // Nothing decoded and nothing lies above: rows exist up there all the
    // same, so this is neither the live tail nor an older numbering.
    await conv.catchUp(3);
    assertEquals(seen.filter((o) => o.limit === 1).length, 0);
    assertEquals(conv.lastSeq, 13);

    await conv.catchUp(3);
    assertEquals(seen[seen.length - 1]?.afterSeq, 13);
    assertEquals(conv.lastSeq, 16);
  },
);

Deno.test("a page whose edge does not move ends the sweep", async () => {
  const { conv, seen } = conversation(() => ({
    rows: [],
    more: true,
    undecodable: 1,
    next: 10,
  }));
  conv.lastSeq = 10;
  await conv.catchUp(3);
  assertEquals(seen, [{ afterSeq: 10, limit: 3 }], "asked once, never again");
  assertEquals(conv.lastSeq, 10, "the cursor stays put with nothing rendered");
});

Deno.test("a live row carries the position its frame names", () => {
  const { conv } = conversation(() => page([], false));
  conv.lastSeq = 10;
  feed(conv, frame(42, 11));
  const line = conv.transcript.lines.find((l) => l.historyId === 42);
  assertEquals(line?.seq, 11, "the rendered row knows where it sits");
  assertEquals(conv.lastSeq, 11);
});

Deno.test(
  "a page names the position of a row the stream rendered without one",
  async () => {
    const { conv } = conversation(() => page([row(42, 11)], false));
    conv.lastSeq = 10;
    feed(conv, frame(42));
    assertEquals(
      conv.transcript.lines[0]!.seq,
      undefined,
      "an unstamped frame names no position",
    );
    await conv.catchUp(1);
    assertEquals(
      conv.transcript.lines[0]!.seq,
      11,
      "the page fills the hole in place",
    );
    assertEquals(
      conv.transcript.lines.length,
      1,
      "the same row, not a second copy",
    );
  },
);
