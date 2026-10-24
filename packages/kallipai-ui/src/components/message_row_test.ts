// Source-read pins for the row's bubble and the aside the bubble carries
// (same rationale as message_actions_test: a layout wiring a type-check cannot
// see). Two shapes a type-check cannot catch:
//
//   - every chat surface draws its bubble through one shared row, and the quote
//     control reaches the bubble only as the aside that surface hands in -- a
//     second placement is how the surfaces drift apart;
//   - the row carries the bubble's width cap and its sender-side alignment and
//     nothing else: anchor the aside to the row instead of the bubble's own box
//     and the controls centre on the bubble plus its action row, landing under
//     it; where the aside lands is pinned in message_bubble_test.
//
// Not pinned, on purpose: the row's class strings beyond the width cap and the
// alignment (a restyle is not a defect), and the aside's own geometry
// (vertical centring is read on a deployed build).

import { assert } from "@std/assert";

const ROW = new URL("./MessageRow.svelte", import.meta.url);
const AFFORDANCE = new URL("./RowQuoteAffordance.svelte", import.meta.url);
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
  const row = source(ROW);
  assert(
    !row.includes("RowQuoteAffordance"),
    "the row places no quote control: the aside comes in through the bubble",
  );
  for (const [label, url] of [
    ["direct and channel", VIEW],
    ["room", ROOM],
  ] as const) {
    const src = source(url);
    assert(
      src.includes('import MessageRow from "') && src.includes("<MessageRow"),
      `${label} draw the bubble through the shared row`,
    );
    assert(
      src.includes('import RowQuoteAffordance from "'),
      `${label} build the aside they hand down`,
    );
    const open = src.indexOf("{#snippet beside()}");
    const close = src.indexOf("{/snippet}", open);
    assert(open >= 0 && close > open, `${label} declare a beside snippet`);
    const at = src.indexOf("<RowQuoteAffordance", open);
    assert(
      at > open && at < close,
      `${label} build the aside inside the snippet the bubble renders`,
    );
    assert(
      src.indexOf("<RowQuoteAffordance", at + 1) === -1,
      `${label} place the aside once`,
    );
    assert(
      src.includes("{beside}"),
      `${label} hand the aside to the bubble that renders it`,
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

Deno.test("the aside spends no layout: it hangs off the bubble's box", () => {
  const src = source(AFFORDANCE);
  const actions = src.indexOf("<MessageActions");
  assert(actions >= 0, "the aside renders the shared controls");
  const wrapper = openingTagOf(src, actions);
  assert(
    wrapper.includes("absolute"),
    "out of flow, so the controls cost the bubble no layout",
  );
});
