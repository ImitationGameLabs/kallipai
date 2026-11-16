// The one decision behind a clickable bubble body: an inner control and a text
// selection both keep their click, everything else belongs to the bubble. The
// cases are the four ways a real click arrives, plus the two quiet ones (a
// caret, and no selection at all).

import { assertEquals } from "@std/assert";
import {
  bubbleActivates,
  INNER_CONTROL,
  selectionHoldsText,
} from "./bubble_click.ts";

const inside = (hit: unknown) => ({ closest: (_selector: string) => hit });
const plain = inside(null);
const box = { contains: (_node: unknown) => true };
const elsewhere = { contains: (_node: unknown) => false };

Deno.test("an inner control keeps its own click", () => {
  assertEquals(bubbleActivates(inside({ tag: "button" }), null, box), false);
});

Deno.test("a link keeps its own click", () => {
  assertEquals(bubbleActivates(inside({ tag: "a" }), null, box), false);
});

Deno.test("a selection inside the bubble is a copy, not a click", () => {
  assertEquals(
    bubbleActivates(plain, { collapsed: false, anchorInBox: true }, box),
    false,
  );
});

Deno.test("an empty click on the body is the bubble's", () => {
  assertEquals(bubbleActivates(plain, null, box), true);
});

Deno.test("a caret somewhere in the bubble still lets it open", () => {
  assertEquals(
    bubbleActivates(plain, { collapsed: true, anchorInBox: true }, box),
    true,
  );
});

Deno.test("a selection outside the bubble is not ours to read", () => {
  assertEquals(
    bubbleActivates(plain, { collapsed: false, anchorInBox: false }, box),
    true,
  );
});

Deno.test("a target with no element still counts as the body", () => {
  assertEquals(bubbleActivates(null, null, box), true);
});

Deno.test("a control above the bubble is not the bubble's own", () => {
  // A button wrapping the row, or a panel header, must not make the whole
  // bubble dead: only controls the box holds answer their own clicks.
  assertEquals(
    bubbleActivates(inside({ tag: "button" }), null, elsewhere),
    true,
  );
});

Deno.test("selectionHoldsText reads a copy, not a caret", () => {
  assertEquals(selectionHoldsText(null), false);
  assertEquals(
    selectionHoldsText({ collapsed: true, anchorInBox: true }),
    false,
  );
  assertEquals(
    selectionHoldsText({ collapsed: false, anchorInBox: false }),
    false,
  );
  assertEquals(
    selectionHoldsText({ collapsed: false, anchorInBox: true }),
    true,
  );
});

const BUBBLE = new URL("../components/MessageBubble.svelte", import.meta.url);

Deno.test(
  "every control the bubble draws is filtered as an inner control",
  async () => {
    // The filter is what keeps a control's own click its own. A literal control
    // the bubble grows later has to join it, or the bubble swallows the click.
    const source = await Deno.readTextFile(BUBBLE);
    const filter = INNER_CONTROL.split(", ");
    const tags = [
      ...source.matchAll(/<(button|input|textarea|select|a)\b/g),
    ].map((found) => found[1]);
    const roles = [...source.matchAll(/role=.(button|link)./g)].map(
      (found) => `[role='${found[1]}']`,
    );
    // Named one by one, taken from what the bubble renders today: a filter
    // that loses one of these fails here, before any new control arrives.
    for (const kind of ["button", "input", "textarea", "select", "a"]) {
      assertEquals(filter.includes(kind), true, kind);
    }
    for (const role of ["[role='button']", "[role='link']"]) {
      assertEquals(filter.includes(role), true, role);
    }
    // Whatever the scan finds has to be filtered too.
    assertEquals(tags.length > 0, true, "the bubble draws controls");
    for (const selector of [...tags, ...roles]) {
      assertEquals(filter.includes(selector), true, selector);
    }
  },
);
