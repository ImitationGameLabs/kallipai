// The gap pump: a `gap` frame says the direct stream lost frames, so the
// conversation pulls the hole back through the history cursor instead of
// waiting for the next reconnect to catch it up. These cases pin the anchor
// choice, the paging, and the single-flight contract.
//
// The module under test is rune-bearing and `deno test` runs it uncompiled, so
// a passthrough `$state` shim lets it run with plain fields.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { assertEquals } = await import("@std/assert");
const { LocalConversation, WINDOW_PAGE } =
  await import("./conversation.svelte.ts");
import { DirectTransport } from "./directTransport.ts";
import { DirectMessageSender } from "./messageSender.svelte.ts";
import { LOCAL_OPERATOR_SENDER } from "../transcript.ts";

/** The class behind the dynamic import above (a value, so its instance type
 *  is spelled out rather than imported as a type). */
type Conv = InstanceType<typeof LocalConversation>;

type PageRequest = {
  after?: number | null;
  before?: number | null;
  limit?: number;
};

/** One history page as the external face hands it back. */
type Page = {
  rows: ReturnType<typeof userRow>[];
  more: boolean;
  undecodable: number;
};

/** One inbound row as the history face hands it back. */
function userRow(historyId: number) {
  return {
    sender: { id: "u1", kind: "human", handle: "Alice" },
    reply: {
      kind: "user_message",
      history_id: historyId,
      text: `row ${historyId}`,
      created_at: "2026-10-22T00:00:00Z",
    },
  };
}

/** `count` rows starting at `from`, the way an after-page reads them. */
function rowsFrom(from: number, count: number) {
  return Array.from({ length: count }, (_, i) => userRow(from + i));
}

function page(rows: ReturnType<typeof userRow>[], more = false): Page {
  return { rows, more, undecodable: 0 };
}

/** A local conversation over a transport whose history face is scripted. The
 *  store hands the conversation back so the live-entry guard holds. */
function harness(handler: (opts: PageRequest) => Page | Promise<Page>) {
  const seen: PageRequest[] = [];
  const holder: { conv?: Conv } = {};
  const client = {
    externalEventStream: () => {
      throw new Error("these cases never open the stream");
    },
    externalHistory: (_id: string, opts: PageRequest) => {
      seen.push({ ...opts });
      return Promise.resolve(handler(opts));
    },
  };
  const transport = new DirectTransport(
    client as never,
    "root",
    LOCAL_OPERATOR_SENDER,
  );
  const conv = new LocalConversation(
    { get: () => holder.conv } as never,
    transport,
    "conv-local",
    new DirectMessageSender(client as never, "root"),
  );
  holder.conv = conv;
  return { conv, seen };
}

function ids(conv: Conv): number[] {
  return conv.transcript.lines.map((l) => l.historyId).filter((id) => id > 0);
}

Deno.test("a gap frame pulls from its cursor and fills the hole", async () => {
  const { conv, seen } = harness(() => page(rowsFrom(8, 2)));
  conv.maxRendered = 7;
  await conv.onGapFrame(7);
  assertEquals(
    seen.map((o) => o.after),
    [7],
  );
  assertEquals(ids(conv), [8, 9]);
  assertEquals(conv.maxRendered, 9);
  assertEquals(conv.minRendered, 8);
});

Deno.test("a hole wider than one page is paged to the newest row", async () => {
  // The surviving tail renders before the first page lands, so the window
  // head sits far above the hole; the sweep has to follow the page's last id
  // instead, or one page would end it with the hole still open.
  const { conv, seen } = harness((opts) => {
    const after = opts.after ?? 0;
    const count = Math.min(WINDOW_PAGE, 199 - after);
    return page(rowsFrom(after + 1, count), after + count < 199);
  });
  conv.maxRendered = 200;
  await conv.onGapFrame(7);
  assertEquals(
    seen.map((o) => o.after),
    [7, 57, 107, 157],
  );
  assertEquals(ids(conv).length, 192);
  assertEquals(conv.maxRendered, 200);
});

Deno.test("a gap without a cursor pulls from the window head", async () => {
  // The stream delivered no durable row, so the window's high-water mark is
  // the only anchor that cannot ask for rows already on screen.
  const { conv, seen } = harness(() => page(rowsFrom(43, 2)));
  conv.maxRendered = 42;
  await conv.onGapFrame(null);
  assertEquals(
    seen.map((o) => o.after),
    [42],
  );
  assertEquals(conv.maxRendered, 44);
});

Deno.test(
  "a gap with no cursor and an empty window takes the newest page",
  async () => {
    // Nothing rendered yet: there is no cursor to page from, so the recent-N
    // snapshot stands in. A batch short of a page is the whole history.
    const { conv, seen } = harness(() => page(rowsFrom(5, 3)));
    await conv.onGapFrame(null);
    assertEquals(
      seen.map((o) => o.after),
      [undefined],
    );
    assertEquals(ids(conv), [5, 6, 7]);
    assertEquals(conv.hasMoreOlder, false);
  },
);

Deno.test(
  "a gap during a fill queues one sweep, not a second fill",
  async () => {
    let release: (p: Page) => void = () => {};
    const held = new Promise<Page>((resolve) => {
      release = resolve;
    });
    const { conv, seen } = harness((opts) => {
      if (opts.after === 7) return held;
      return page(rowsFrom(4, 4));
    });
    conv.maxRendered = 7;
    const first = conv.onGapFrame(7);
    await Promise.resolve();
    // A second gap lands mid-fill: it starts nothing yet.
    await conv.onGapFrame(4);
    assertEquals(seen.length, 1);
    release(page(rowsFrom(8, 2)));
    await first;
    // The queued hole is smaller than the one just filled, so one more sweep
    // runs from it.
    assertEquals(
      seen.map((o) => o.after),
      [7, 4],
    );
    assertEquals(ids(conv), [4, 5, 6, 7, 8, 9]);
  },
);

Deno.test("a failed pull leaves the hole for the next catch-up", async () => {
  // A pull that throws is not a filled hole: gap frames do not repeat on
  // their own, so the anchor has to outlive the failure.
  let fail = true;
  const { conv, seen } = harness(() => {
    if (fail) throw new Error("server unreachable");
    return page(rowsFrom(8, 2));
  });
  conv.maxRendered = 7;
  await conv.onGapFrame(7);
  assertEquals(ids(conv), []);
  // The reconnect renders the survivors far above the hole; a catch-up from
  // that head would step straight over it.
  conv.maxRendered = 200;
  fail = false;
  await conv.catchUp();
  assertEquals(
    seen.map((o) => o.after),
    [7, 7],
  );
  assertEquals(ids(conv), [8, 9]);
});

Deno.test("an empty page that ends the history is not a failure", async () => {
  // Catch-up runs on every mount and reconnect, and an empty page past the
  // newest row is its ordinary outcome; reporting it would cry wolf each time.
  const errors: unknown[][] = [];
  const original = console.error;
  console.error = (...args: unknown[]) => errors.push(args);
  try {
    const { conv } = harness(() => page([]));
    conv.maxRendered = 7;
    await conv.catchUp();
    assertEquals(errors, []);
  } finally {
    console.error = original;
  }
});

Deno.test("a page that claims more but decodes nothing is named", async () => {
  // `more: true` with no decodable row means the server filled the page with
  // rows this client could not decode: that is the case worth a line.
  const errors: unknown[][] = [];
  const original = console.error;
  console.error = (...args: unknown[]) => errors.push(args);
  try {
    const { conv } = harness(() => page([], true));
    conv.maxRendered = 7;
    await conv.catchUp();
    assertEquals(errors.length, 1);
  } finally {
    console.error = original;
  }
});

Deno.test("a settled anchor is not swept a second time", async () => {
  // The failed attempt leaves its own anchor owed. Once a retry covers that
  // anchor, it is settled: the next gap frame must not pull it again.
  let fail = true;
  const { conv, seen } = harness(() => {
    if (fail) throw new Error("server unreachable");
    return page(rowsFrom(8, 2));
  });
  conv.maxRendered = 7;
  await conv.onGapFrame(7);
  assertEquals(ids(conv), []);
  fail = false;
  await conv.onGapFrame(7);
  assertEquals(
    seen.map((o) => o.after),
    [7, 7],
  );
  assertEquals(ids(conv), [8, 9]);
});

Deno.test("the smallest queued anchor wins the re-run", async () => {
  let release: (p: Page) => void = () => {};
  const held = new Promise<Page>((resolve) => {
    release = resolve;
  });
  const { conv, seen } = harness((opts) => {
    if (opts.after === 100) return held;
    return page(rowsFrom(41, 2));
  });
  conv.maxRendered = 100;
  const first = conv.onGapFrame(100);
  await Promise.resolve();
  await conv.onGapFrame(60);
  await conv.onGapFrame(40);
  assertEquals(seen.length, 1, "the running fill owns the pull");
  release(page(rowsFrom(101, 2)));
  await first;
  assertEquals(
    seen.map((o) => o.after),
    [100, 40],
  );
  assertEquals(ids(conv), [41, 42, 101, 102]);
});

Deno.test("a closed conversation starts no fill", async () => {
  const { conv, seen } = harness(() => page(rowsFrom(8, 2)));
  conv.maxRendered = 7;
  conv.close();
  await conv.onGapFrame(7);
  assertEquals(seen.length, 0, "nothing lands on a conversation nobody reads");
});

Deno.test("a conversation closed mid-fill takes no more sweeps", async () => {
  let release: (p: Page) => void = () => {};
  const held = new Promise<Page>((resolve) => {
    release = resolve;
  });
  const { conv, seen } = harness((opts) => {
    if (opts.after === 7) return held;
    return page(rowsFrom(4, 2));
  });
  conv.maxRendered = 7;
  const first = conv.onGapFrame(7);
  await Promise.resolve();
  await conv.onGapFrame(4);
  conv.close();
  release(page(rowsFrom(8, 2)));
  await first;
  assertEquals(
    seen.map((o) => o.after),
    [7],
    "the queued sweep never runs",
  );
});

Deno.test("a gap during mount catch-up lands each row once", async () => {
  // The mount catch-up and the gap fill ask the same question: every pulled
  // row merges by id, so the transcript holds it once whichever lands first.
  let release: (p: Page) => void = () => {};
  const held = new Promise<Page>((resolve) => {
    release = resolve;
  });
  const { conv, seen } = harness(() => held);
  conv.maxRendered = 7;
  const catching = conv.catchUp(3);
  await Promise.resolve();
  const filling = conv.onGapFrame(7);
  release(page(rowsFrom(8, 2)));
  await Promise.all([catching, filling]);
  assertEquals(seen.length, 2, "both asked; the rows still land once");
  assertEquals(ids(conv), [8, 9]);
});
