// Source-read pins for the row's bubble (same rationale as quote_action_test:
// a layout wiring a type-check cannot see). Two shapes a type-check cannot
// catch:
//
//   - every chat surface draws its bubble through one shared row, so the
//     direct, room and channel shapes cannot drift apart again;
//   - the row carries the bubble's width cap and its sender-side alignment
//     and nothing else -- a second cap or a second alignment is how a bubble
//     ends up wider than the transcript allows, or on the wrong side.

import { assert } from "@std/assert";

const ROW = new URL("./MessageRow.svelte", import.meta.url);
const VIEW = new URL("./ConversationView.svelte", import.meta.url);
const ROOM = new URL("../pages/RoomConversationPage.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

/** The opening tag of the element that contains `at`. */
function openingTagOf(src: string, at: number): string {
  const open = src.lastIndexOf("<div", at);
  assert(open >= 0, "the bubble is not inside an element");
  const close = src.indexOf(">", open);
  assert(close > open, "that element never closes its opening tag");
  return src.slice(open, close + 1);
}

Deno.test("every chat surface draws its bubble through the shared row", () => {
  for (const [label, url] of [
    ["direct and channel", VIEW],
    ["room", ROOM],
  ] as const) {
    const src = source(url);
    assert(
      src.includes('import MessageRow from "') && src.includes("<MessageRow"),
      `${label} draw the bubble through the shared row`,
    );
  }
});

Deno.test(
  "the row caps the width and aligns the bubble -- nothing else",
  () => {
    const row = source(ROW);
    const bubble = row.indexOf("{@render children()}");
    assert(bubble >= 0, "the row renders the bubble");
    const wrapper = openingTagOf(row, bubble);
    assert(
      /max-w-\[/u.test(wrapper),
      "the row wrapper is the bubble's width cap, not the full-width transcript",
    );
    assert(
      /items-(end|start)/u.test(wrapper),
      "the row wrapper is what puts the bubble on its sender's side",
    );
    assert(
      !row.includes("absolute"),
      "nothing is positioned against the row wrapper",
    );
  },
);
