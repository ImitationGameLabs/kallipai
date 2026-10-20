// Source-read pins for the composer's pending-quote strip: the strip is where
// the user sees what a send will quote, and its wiring (label fallback,
// preview, cancel) is markup a typecheck cannot check.

import { assert } from "@std/assert";

const BAR = new URL("./QuoteBar.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the strip shows the author, the quoted body, and the way out",
  { permissions: { read: [BAR] } },
  () => {
    const src = source(BAR);
    assert(src.includes("author ?? chat_quote_unknown_author()"));
    assert(src.includes("{preview}"), "the strip renders the quoted body");
    assert(src.includes("aria-label={chat_quote_cancel_aria()}"));
    assert(src.includes("onclick={onCancel}"));
  },
);

Deno.test(
  "the strip names itself and its preview for assistive tech",
  { permissions: { read: [BAR] } },
  () => {
    const src = source(BAR);
    assert(src.includes('role="group"'));
    assert(src.includes("chat_quote_bar_aria({ author: label })"));
    assert(src.includes("{chat_quote_bar_preview_aria()}"));
  },
);
