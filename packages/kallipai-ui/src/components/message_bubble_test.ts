// Source-read pins for the message file card (same rationale as
// attachment_bar_test: the card's contract is presentation wiring that a
// typecheck cannot see). The bubble stays a dumb renderer: it shows the
// reference and hands the click to the page-supplied download I/O.

import { assert } from "@std/assert";

const BUBBLE = new URL("./MessageBubble.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "the file card renders only under the attachment guard",
  { permissions: { read: [BUBBLE] } },
  () => {
    const src = source(BUBBLE);
    const guard = src.indexOf("{#if attachment}");
    assert(guard >= 0, "the card must be guarded on the attachment");
    // The card block (name/size/button) all sit inside the guard, so a
    // plain message renders byte-for-byte as before.
    const body = src.slice(guard);
    assert(body.includes("attachment.name"));
    assert(body.includes("attachment.size"));
    assert(body.indexOf("{/if}") < body.indexOf("{#if markdown}"));
  },
);

Deno.test(
  "the download button wires the page-supplied I/O and the inline failure",
  { permissions: { read: [BUBBLE] } },
  () => {
    const src = source(BUBBLE);
    assert(src.includes("onclick={download}"));
    assert(src.includes("chat_file_download_aria({ name: attachment.name })"));
    // A failed download degrades to the inline unavailable message.
    assert(src.includes("downloadFailed = true"));
    assert(src.includes("chat_file_unreadable()"));
  },
);
