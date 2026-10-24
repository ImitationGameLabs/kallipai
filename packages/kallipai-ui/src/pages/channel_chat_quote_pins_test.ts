// Source-read pins for the chat page's quoting wiring. The controller unit
// tests cover the state machine and the component pin covers the row; what is
// left is the page, where a gate can quietly disappear (the retry seam handed
// to a conversation that has no probe to run, or a quote id that stops riding
// the send).

import { assert } from "@std/assert";
import { flat } from "../test_support/flat.ts";

const PAGE = new URL("./ChannelChatPage.svelte", import.meta.url);
const VIEW = new URL("../components/ConversationView.svelte", import.meta.url);
const QUOTES = new URL("../lib/quotes.svelte.ts", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the page owns one quoting controller and syncs it from content",
  () => {
    const src = flat(source(PAGE));
    assert(
      src.includes("const quotes = createQuotes({ conversation: () => conv })"),
      "one controller, reading the live conversation",
    );
    assert(
      src.includes("$effect(() => quotes.sync())"),
      "unresolved targets are probed when rows land",
    );
    assert(
      src.includes("{#if quotes.quotedView}") && src.includes("<QuoteBar"),
      "the bar draws only while a target is armed",
    );
  },
);

Deno.test("a leaf with neither capability is refused quoting", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes(
      "const canQuote = $derived(!(conv instanceof OfflineConversation))",
    ),
    "the gate stands for both capabilities, not a list of leaves",
  );
  for (const seam of [
    "onQuote={canQuote ? quotes.quote : undefined}",
    "onQuoteRetry={canQuote ? quotes.retry : undefined}",
  ]) {
    assert(src.includes(seam), `missing seam: ${seam}`);
  }
  assert(
    src.includes("fileButton={conv instanceof RelayConversation"),
    "attachments still ride the relay transport only",
  );
});

Deno.test("one send spends one quote id", () => {
  const src = flat(source(PAGE));
  const arming = src.indexOf("const quoteId = quotes.sendQuote();");
  assert(arming !== -1, "the submit captures the armed id up front");
  const after = src.slice(arming, arming + 200);
  assert(
    after.includes("quotes.cancel()"),
    "the strip is spent by the submit that captured it",
  );
  assert(
    src.includes("{ quote: quoteId }") &&
      src.includes("{ attachment: first, quote: quoteId }"),
    "the id travels with the first message only",
  );
  assert(
    src.includes("{ attachment: att }"),
    "attachment-only follow-ups carry no quote",
  );
});
Deno.test("the armed id has no second way out of the controller", () => {
  const src = source(QUOTES);
  // The real regression is a second outlet put back beside the send: an
  // accessor a caller can read instead of going through sendQuote().
  assert(
    !/get quoted\b/u.test(src),
    "no second accessor for the id a send carries",
  );
});

Deno.test(
  "the transcript receives the controller's seams and the quote view",
  () => {
    const src = flat(source(PAGE));
    for (const seam of [
      "onQuote={canQuote ? quotes.quote : undefined}",
      "resolveQuote={quotes.resolve}",
      "onCancelQuote={() => quotes.cancel()}",
    ]) {
      assert(src.includes(seam), `missing seam: ${seam}`);
    }
  },
);

Deno.test(
  "a probed row arrives with jump-off, so the block does not promise a scroll",
  () => {
    const src = flat(source(VIEW));
    assert(
      src.includes(
        "{@const quoteTarget = quoted?.jumpable === true ? quotedId : undefined}",
      ),
      "one judgement decides whether a target has a row",
    );
    assert(
      src.includes(
        "quoteTarget !== undefined ? () => reveal.jump(quoteTarget) : undefined}",
      ),
      "the jump exists only for that target",
    );
    assert(
      src.includes("onQuoteClick={quoteJump}"),
      "and the bubble offers exactly that jump",
    );
  },
);

Deno.test("a row's anchor and the quote block name the same row", () => {
  const src = flat(source(VIEW));
  assert(
    src.includes('<span id="msg-a11y-{line.historyId}" class="sr-only">'),
    "each row stamps its own id as the anchor a quote points at",
  );
  assert(
    src.includes("`msg-a11y-${quoteTarget}`"),
    "and the quote block points at the target row's anchor",
  );
});
