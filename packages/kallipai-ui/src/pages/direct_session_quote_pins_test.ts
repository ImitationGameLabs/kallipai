// Source-read pins for the direct-session page's quoting wiring. The lookup
// itself is behaviour-tested (quote_test); what is left is the page, where the
// wiring can quietly go missing: a row whose quote never leaves `toLine`, or a
// resolved view that never reaches the bubble.

import { assert } from "@std/assert";
import { flat } from "../test_support/flat.ts";

const PAGE = new URL("./DirectSessionPage.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test("the direct page resolves the window's quotes in one pass", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("directQuoteBlocksByTarget(lines)"),
    "one pass over the loaded window, not one lookup per row",
  );
});

Deno.test("a direct row keeps the quote it was stamped with", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("quote: row.quote"),
    "the row's quote survives the projection onto the transcript line",
  );
});

Deno.test("the bubble's quote id resolves against the block table", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("resolveQuote={(quotedSeq) => quotes.get(quotedSeq)}"),
    "the table is keyed by the seq the bubble asks for",
  );
});

Deno.test("a direct window offers no quote retry", () => {
  const src = flat(source(PAGE));
  assert(
    !src.includes("onQuoteRetry"),
    "a window with no probe has nothing to retry",
  );
});

Deno.test("an own row keeps the handle the wire stamped on it", () => {
  const src = flat(source(PAGE));
  assert(
    src.includes("? row.sender.handle"),
    "a quoted own row names its author instead of drawing a blank line",
  );
});
