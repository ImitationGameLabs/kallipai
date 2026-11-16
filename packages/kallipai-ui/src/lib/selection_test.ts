// Pins for the one rule every clickable bubble reads: a drag that started
// inside the box was the reader copying, so the box does not act on it.

import { assertEquals } from "@std/assert";
import { selectionAtClickOf } from "./selection.ts";

const anchor = "anchor-node";
const holdsIt = { contains: (node: unknown) => node === anchor };
const holdsNothing = { contains: () => false };
const drag = { isCollapsed: false, anchorNode: anchor };
const caret = { isCollapsed: true, anchorNode: anchor };

Deno.test("no selection at all reads as none", () => {
  assertEquals(selectionAtClickOf(null, holdsIt), null);
});

Deno.test("a drag that started inside the box is the reader copying", () => {
  assertEquals(selectionAtClickOf(drag, holdsIt), {
    collapsed: false,
    anchorInBox: true,
  });
});

Deno.test("a drag that started elsewhere leaves the box acting", () => {
  assertEquals(selectionAtClickOf(drag, holdsNothing), {
    collapsed: false,
    anchorInBox: false,
  });
});

Deno.test("a selection with no anchor starts nowhere", () => {
  assertEquals(
    selectionAtClickOf({ isCollapsed: false, anchorNode: null }, holdsIt),
    {
      collapsed: false,
      anchorInBox: false,
    },
  );
});

Deno.test("a box the click never named holds nothing", () => {
  assertEquals(selectionAtClickOf(drag, null), {
    collapsed: false,
    anchorInBox: false,
  });
});

Deno.test("a caret inside the box is not a selection", () => {
  assertEquals(selectionAtClickOf(caret, holdsIt), {
    collapsed: true,
    anchorInBox: true,
  });
});
