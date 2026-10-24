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

/**
 * Index of the `</div>` closing the `<div` opened at `open`.
 */
function closingDiv(src: string, open: number): number {
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src.startsWith("<div", i)) depth++;
    else if (src.startsWith("</div>", i)) {
      depth--;
      if (depth === 0) return i;
    }
  }
  return -1;
}

Deno.test(
  "the aside renders inside the bubble's own anchored box",
  { permissions: { read: [BUBBLE] } },
  () => {
    const src = source(BUBBLE);
    const box = src.indexOf("bind:this={box}");
    const aside = src.indexOf("{@render beside()}");
    assert(box >= 0, "the bubble renders its own box");
    assert(aside > box, "the aside renders right after the bubble");
    // `bind:this={box}` sits inside the bubble's own element: step out of it
    // once to reach the box the aside is meant to measure.
    const bubbleOpen = src.lastIndexOf("<div", box);
    const anchor = src.lastIndexOf("<div", bubbleOpen - 1);
    assert(anchor >= 0 && bubbleOpen > anchor, "the bubble sits in a box");
    const anchorEnd = closingDiv(src, anchor);
    assert(anchor >= 0 && anchorEnd > aside, "the aside is inside that box");
    const tag = src.slice(anchor, src.indexOf(">", anchor));
    assert(tag.includes("relative"), "the box the aside positions against");
    assert(
      src.includes("{#if beside}"),
      "a surface that brings no aside renders the bubble as before",
    );
    // Outside the anchored box, so it cannot move what hangs beside the bubble.
    assert(
      src.indexOf("bind:this={actions}") > anchorEnd,
      "the action row stays under the bubble, out of the aside's box",
    );
  },
);

Deno.test(
  "the action row is still the bubble's, under the bare guard",
  { permissions: { read: [BUBBLE] } },
  () => {
    const src = source(BUBBLE);
    const guard = src.indexOf("{#if !bare}");
    assert(guard >= 0, "the row hides for a bare bubble");
    const body = src.slice(guard);
    assert(body.includes("bind:this={actions}"));
    assert(body.includes("<CopyButton"));
  },
);
