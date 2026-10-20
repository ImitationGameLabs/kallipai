// Source-read pins for the quote block (same rationale as
// message_bubble_test: the block's contract is presentation wiring that a
// typecheck cannot see -- an empty preview span type-checks fine and shipped
// once, leaving the block with an author and a blank line).

import { assert } from "@std/assert";

const BLOCK = new URL("./QuoteBlock.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

function occurrences(hay: string, needle: string): number {
  return hay.split(needle).length - 1;
}

/** The found state's own markup: from its branch opener to the block's
 *  else (the two out-of-window states). */
function foundSection(src: string): string {
  const open = src.indexOf('{#if view.status === "found"}');
  const close = src.indexOf("{:else}", open);
  assert(open !== -1 && close > open, "the found state has its own section");
  return src.slice(open, close);
}

Deno.test(
  "the found block renders the quoted body, not an empty line",
  { permissions: { read: [BLOCK] } },
  () => {
    const src = source(BLOCK);
    // Contract, not a layout count: whichever branch the found state grows,
    // every branch that draws the author line also draws the body -- a branch
    // that drops it fails here instead of shipping a blank line.
    const found = foundSection(src);
    const branches = occurrences(
      found,
      "{view.author ?? chat_quote_unknown_author()}",
    );
    assert(branches > 0, "the found state draws the author line");
    assert(
      occurrences(found, "{view.preview}") >= branches,
      `every found branch shows the quoted body: ${branches}`,
    );
    // The author line keeps its fallback for rows cached before senders.
    assert(src.includes("chat_quote_unknown_author()"));
  },
);

Deno.test(
  "the four states stay distinct and only unavailable offers a retry",
  { permissions: { read: [BLOCK] } },
  () => {
    const src = source(BLOCK);
    assert(src.includes('{#if view.status === "found"}'));
    const retry = src.indexOf('view.status === "unavailable" && onRetry');
    assert(retry >= 0, "retry is gated on the unavailable state");
    assert(src.indexOf("chat_quote_deleted()") < retry);
    assert(src.indexOf("chat_quote_unavailable()") < retry);
    assert(src.indexOf("chat_quote_off_window()") < retry);
    assert(
      src.includes('view.status === "off-window"'),
      "an off-window target renders inert copy, never a retry",
    );
  },
);

Deno.test(
  "a described label does not hide the quoted body from the reader",
  { permissions: { read: [BLOCK] } },
  () => {
    const src = source(BLOCK);
    // The label points at the quoted row's own label, which names the author
    // and the fact of quoting -- never the body. Hiding the preview then
    // leaves the reader with no way to hear what was quoted.
    const found = foundSection(src);
    const hidden = occurrences(src, "aria-hidden={describedBy !== undefined}");
    assert(
      occurrences(found, "{view.preview}") > 0,
      "the found state shows the quoted body",
    );
    assert(
      hidden === 0,
      "the preview must stay readable when describedBy is set",
    );
    assert(src.includes("aria-describedby={describedBy}"));
  },
);
