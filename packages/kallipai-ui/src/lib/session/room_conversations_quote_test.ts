// Source-read pins for the room send path's quote: the three write sites that
// must agree. A quote is settled once (so the optimistic line and the wire
// carry the same seq), the optimistic line carries it, and a retry re-sends it
// -- dropping it there would turn a retried reply into an unquoted one.

import { assert } from "@std/assert";
import { flat } from "../../test_support/flat.ts";

const STORE = new URL("./roomConversations.svelte.ts", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test("the store settles a quote the way it settles mentions", () => {
  const src = flat(source(STORE));
  assert(
    src.includes("const quote = checkedQuote(options?.quote)"),
    "one settle point, before the optimistic line is drawn",
  );
});

Deno.test("the optimistic line carries the quote it was sent with", () => {
  const src = flat(source(STORE));
  assert(
    src.includes("...quoteField(quote)"),
    "the bubble shows the quote from the instant it appears",
  );
});

Deno.test("the payload carries the settled quote", () => {
  const src = flat(source(STORE));
  assert(
    src.includes("encodeRoomSendMessage(trimmed, { mentions, quote })"),
    "the wire gets the same seq the optimistic line drew",
  );
});

Deno.test("a retry re-sends the quote with the text", () => {
  const src = flat(source(STORE));
  assert(src.includes("quote: line.quote"), "a retried reply stays a reply");
});
