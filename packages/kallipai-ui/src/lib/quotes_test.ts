// Unit tests for the quoting state machine: arming a line, the single probe
// each unresolved target gets, and the three outcomes a probe can settle as.
// The module is rune-bearing and `deno test` runs it uncompiled, so it loads
// behind a passthrough $state shim plus an ambient declaration (the seam
// cache_quote_test and relayWindow_test use).

import { assertEquals } from "@std/assert";
import type { ConversationBase } from "./session/conversation.svelte.ts";
import type { ConversationLine } from "./transcript.ts";

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

const { createQuotes } = await import("./quotes.svelte.ts");

function line(over: Partial<ConversationLine>): ConversationLine {
  return { historyId: 1, role: "user", text: "hello", ...over };
}

type Peek = (id: number) => Promise<ConversationLine | "deleted" | "error">;

/** A conversation stub: the loaded window plus the one seam the controller
 *  reads. Cast through the base type -- the controller touches nothing else. */
function stub(lines: ConversationLine[], peekRow: Peek): ConversationBase {
  return { transcript: { lines }, peekRow } as unknown as ConversationBase;
}

/** Let a pending probe settle (it awaits one promise). */
async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

Deno.test("an armed line draws the quote bar from the loaded window", () => {
  const lines = [
    line({
      historyId: 3,
      text: "first",
      sender: { kind: "agent", id: "a", handle: "ada" },
    }),
    line({ historyId: 5, text: "second" }),
  ];
  const conv = stub(lines, noPeek);
  const quotes = createQuotes({ conversation: () => conv });
  assertEquals(quotes.sendQuote(), undefined);
  assertEquals(quotes.quotedView, undefined);
  quotes.quote(lines[0]!);
  assertEquals(quotes.sendQuote(), 3);
  assertEquals(quotes.quotedView, {
    status: "found",
    author: "ada",
    jumpable: true,
    preview: "first",
  });
  quotes.cancel();
  assertEquals(quotes.sendQuote(), undefined);
  assertEquals(quotes.quotedView, undefined);
});

Deno.test(
  "sync probes each unresolved id once, and the answer previews without jumping",
  async () => {
    const asked: number[] = [];
    const conv = stub(
      [line({ historyId: 10, text: "reply", quote: 2 })],
      (id) => {
        asked.push(id);
        return Promise.resolve(line({ historyId: 2, text: "the target" }));
      },
    );
    const quotes = createQuotes({ conversation: () => conv });
    quotes.sync();
    quotes.sync(); // in flight: no second request
    assertEquals(asked, [2]);
    assertEquals(quotes.resolve(2), undefined); // nothing is guessed while pending
    await settle();
    assertEquals(quotes.resolve(2), {
      status: "found",
      author: undefined,
      jumpable: false,
      preview: "the target",
    });
    quotes.sync(); // a settled answer is never re-asked
    assertEquals(asked, [2]);
  },
);

Deno.test(
  "a probe that finds nothing at or below the id settles as deleted",
  async () => {
    const asked: number[] = [];
    const conv = stub([line({ historyId: 10, quote: 4 })], (id) => {
      asked.push(id);
      return Promise.resolve("deleted");
    });
    const quotes = createQuotes({ conversation: () => conv });
    quotes.sync();
    await settle();
    assertEquals(quotes.resolve(4), { status: "deleted" });
    // A fact, not a failure: the block draws no retry, and retry() ignores it.
    quotes.retry(4);
    await settle();
    assertEquals(asked, [4]);
  },
);

Deno.test(
  "a failed probe is unavailable, and only an explicit retry re-asks",
  async () => {
    const asked: number[] = [];
    const conv = stub([line({ historyId: 10, quote: 6 })], (id) => {
      asked.push(id);
      return Promise.resolve(
        asked.length === 1 ? "error" : line({ historyId: 6, text: "target" }),
      );
    });
    const quotes = createQuotes({ conversation: () => conv });
    quotes.sync();
    await settle();
    assertEquals(quotes.resolve(6), { status: "unavailable" });
    quotes.retry(6);
    await settle();
    assertEquals(quotes.resolve(6)?.status, "found");
    assertEquals(quotes.resolve(6)?.jumpable, false);
    assertEquals(asked, [6, 6]);
  },
);

Deno.test(
  "a throwing probe reads as a failed read, not a deleted target",
  async () => {
    const conv = stub([line({ historyId: 10, quote: 8 })], () => {
      throw new Error("channel closed");
    });
    const quotes = createQuotes({ conversation: () => conv });
    quotes.sync();
    await settle();
    assertEquals(quotes.resolve(8), { status: "unavailable" });
  },
);

const noPeek: Peek = () => Promise.resolve("deleted");

/** A conversation whose loaded window can be replaced without swapping the
 *  conversation itself -- what a scroll or a catch-up does in place. */
function slidingStub(
  first: ConversationLine[],
  peekRow: Peek,
): { conv: ConversationBase; slide: (next: ConversationLine[]) => void } {
  const transcript = { lines: first };
  const conv = { transcript, peekRow } as unknown as ConversationBase;
  return {
    conv,
    slide: (next) => {
      transcript.lines = next;
    },
  };
}

Deno.test(
  "a conversation swap drops the armed id and its answers",
  async () => {
    const first = [line({ historyId: 3, text: "first", quote: 2 })];
    const second = [line({ historyId: 41, text: "elsewhere" })];
    const asked: number[] = [];
    const peek: Peek = (id) => {
      asked.push(id);
      return Promise.resolve("deleted");
    };
    let conv = stub(first, peek);
    const quotes = createQuotes({ conversation: () => conv });
    quotes.quote(first[0]!);
    quotes.sync();
    await settle();
    assertEquals(asked, [2]);
    assertEquals(quotes.resolve(2)?.status, "deleted");

    conv = stub(second, peek);
    quotes.sync();

    assertEquals(
      quotes.sendQuote(),
      undefined,
      "row ids do not survive the transcript",
    );
    assertEquals(quotes.quotedView, undefined);
    assertEquals(quotes.resolve(2), undefined, "the old answer went with it");
  },
);

Deno.test(
  "the bar keeps what it was armed from when the row scrolls away",
  async () => {
    const armed = line({
      historyId: 3,
      text: "quoted body",
      sender: { kind: "agent", id: "a", handle: "ada" },
    });
    const { conv, slide } = slidingStub([armed], () =>
      Promise.resolve("deleted"),
    );
    const quotes = createQuotes({ conversation: () => conv });
    quotes.quote(armed);
    assertEquals(quotes.quotedView, {
      status: "found",
      author: "ada",
      jumpable: true,
      preview: "quoted body",
    });

    slide([line({ historyId: 9, text: "later" })]);
    quotes.sync();
    await settle();

    assertEquals(quotes.sendQuote(), 3, "the strip stays armed");
    assertEquals(quotes.quotedView?.author, "ada");
    assertEquals(quotes.quotedView?.preview, "quoted body");
  },
);

Deno.test(
  "a swapped conversation empties the strip and the next send together",
  () => {
    const armed = line({
      historyId: 3,
      text: "quoted body",
      sender: { kind: "agent", id: "a", handle: "ada" },
    });
    let conv = stub([armed], () => Promise.resolve("deleted"));
    const quotes = createQuotes({ conversation: () => conv });
    quotes.quote(armed);
    assertEquals(quotes.quotedView?.author, "ada");
    assertEquals(quotes.sendQuote(), 3, "the strip and the send agree");

    conv = stub([line({ historyId: 41, text: "elsewhere" })], () =>
      Promise.resolve("deleted"),
    );
    // What the page's content effect runs on a swap; under the $state shim
    // the effect body is the same call, so this is its next run.
    quotes.sync();

    assertEquals(quotes.quotedView, undefined, "nothing stays drawn");
    assertEquals(quotes.sendQuote(), undefined, "and nothing can be sent");
  },
);

Deno.test(
  "a send reconciles with the live conversation before answering",
  () => {
    const armed = line({
      historyId: 3,
      text: "quoted body",
      sender: { kind: "agent", id: "a", handle: "ada" },
    });
    let conv = stub([armed], () => Promise.resolve("deleted"));
    const quotes = createQuotes({ conversation: () => conv });
    quotes.quote(armed);

    // The route swapped and no effect has run yet: the send must not reach
    // back into the previous page's row.
    conv = stub([line({ historyId: 41, text: "elsewhere" })], () =>
      Promise.resolve("deleted"),
    );
    assertEquals(quotes.sendQuote(), undefined, "no id crosses the swap");
    assertEquals(quotes.quotedView, undefined, "the entry cleared the strip");
  },
);

Deno.test(
  "a probe answered after the page swapped writes nothing",
  async () => {
    let release!: (row: ConversationLine | "deleted" | "error") => void;
    const pending = new Promise<ConversationLine | "deleted" | "error">(
      (resolve) => {
        release = resolve;
      },
    );
    const convA = stub(
      [line({ historyId: 10, text: "reply", quote: 2 })],
      () => pending,
    );
    const convB = stub([line({ historyId: 20, text: "other" })], noPeek);
    let current: ConversationBase = convA;
    const quotes = createQuotes({ conversation: () => current });
    quotes.sync();
    // The reader moves to another conversation while A's read is in flight.
    current = convB;
    quotes.sync();
    release(line({ historyId: 2, text: "the target" }));
    await settle();
    assertEquals(
      quotes.resolve(2),
      undefined,
      "an answer about the previous conversation must not land in this one",
    );
  },
);

Deno.test("a swap empties the strip before any controller call runs", () => {
  const armedLine = line({
    historyId: 3,
    text: "quoted body",
    sender: { kind: "agent", id: "a", handle: "ada" },
  });
  let conv = stub([armedLine], () => Promise.resolve("deleted"));
  const quotes = createQuotes({ conversation: () => conv });
  quotes.quote(armedLine);
  assertEquals(quotes.quotedView?.author, "ada");

  // The route swapped; nothing has called into the controller since. The
  // strip must not draw what a send would refuse.
  conv = stub([line({ historyId: 41, text: "elsewhere" })], () =>
    Promise.resolve("deleted"),
  );
  assertEquals(quotes.quotedView, undefined, "the strip followed the swap");
  assertEquals(quotes.sendQuote(), undefined, "and the send agrees");
});
