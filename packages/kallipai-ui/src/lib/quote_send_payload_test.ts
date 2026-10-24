// End-to-end check for the quoting send path: the id that rides a message is
// the one the strip shows, and a conversation swap empties both. The unit
// tests judge the controller on its own; this one runs the page's submit shape
// through a real relay conversation down to the transport seam and reads the
// body the wire would carry -- a split between what the reader sees and what
// is sent is the defect it exists for.
//
// Harness: the rune-bearing modules run uncompiled behind a passthrough $state
// shim (the seam quotes_test and retryAllPending_test use); the transport is a
// structural fake that records what it was handed.

declare global {
  function $state<T>(initial: T): T;
  function $state<T>(): T | undefined;
}

(globalThis as Record<string, unknown>)["$state"] = (v: unknown) => v;

type Transport = import("./session/transport.ts").Transport;
type SendOptions = import("./session/transport.ts").SendOptions;
type ConversationLine = import("./transcript.ts").ConversationLine;
type RelayConv = import("./session/conversation.svelte.ts").RelayConversation;

const { assertEquals } = await import("@std/assert");
const { createQuotes } = await import("./quotes.svelte.ts");
const { RelayConversation } = await import("./session/conversation.svelte.ts");

/** A relay conversation whose transport records every send's options. */
function recording(): { conv: RelayConv; sends: SendOptions[] } {
  const sends: SendOptions[] = [];
  const transport = {
    localSender: { kind: "user", id: "u-1", handle: "alice" },
    async *replies() {},
    async *signals() {},
    async *status() {},
    send(_text: string, options?: SendOptions) {
      sends.push(options ?? {});
      return Promise.resolve(1);
    },
    close() {},
  } as unknown as Transport;
  // The store (second argument) is never reached: the paths here stop at the
  // transport seam, and the pending store swallows the missing IndexedDB.
  const conv = new RelayConversation(
    "conv-1",
    null as never,
    transport,
    "tagma-1",
    "T",
  );
  return { conv, sends };
}

/** The body a send request carries. Both shapers drop an undefined quote: the
 *  relay spreads it away, the direct client lets JSON.stringify do it. */
function body(text: string, options: SendOptions): string {
  return JSON.stringify({
    text,
    attachment: options.attachment,
    quote: options.quote,
  });
}

const armed: ConversationLine = {
  historyId: 3,
  role: "user",
  text: "quoted body",
  sender: { kind: "agent", id: "a", handle: "ada" },
};

/** Let the send pump reach the transport (it awaits one promise). */
async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

Deno.test("an armed quote rides the conversation it came from", async () => {
  const { conv, sends } = recording();
  const quotes = createQuotes({ conversation: () => conv });
  quotes.quote(armed);
  assertEquals(quotes.quotedView?.author, "ada");

  // The page's submit, in its own order: one id captured, the strip spent,
  // the send.
  const quoteId = quotes.sendQuote();
  quotes.cancel();
  conv.send("hello", { quote: quoteId });
  await settle();

  assertEquals(
    body("hello", sends[0]!),
    '{"text":"hello","quote":3}',
    "the id rides",
  );
});

Deno.test(
  "a swap between arming and sending leaves the quote out",
  async () => {
    const first = recording();
    const second = recording();
    let current = first.conv;
    const quotes = createQuotes({ conversation: () => current });
    quotes.quote(armed);
    assertEquals(quotes.quotedView?.author, "ada");

    // The route swaps and the reader submits before the page's content effect
    // has run: the send reconciles on its own, so nothing is carried out of a
    // page whose strip no longer belongs to it.
    current = second.conv;
    const quoteId = quotes.sendQuote();
    // What that entry left on screen, read before the strip is spent.
    const strip = quotes.quotedView;

    quotes.cancel();
    second.conv.send("hello", { quote: quoteId });
    await settle();

    assertEquals(
      body("hello", second.sends[0]!),
      '{"text":"hello"}',
      "no quote key",
    );
    assertEquals(strip, undefined, "that entry cleared the strip too");
  },
);
