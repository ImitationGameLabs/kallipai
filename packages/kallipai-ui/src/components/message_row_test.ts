// Source-read pins for the row's bubble (same rationale as quote_action_test:
// a layout wiring a type-check cannot see). Four shapes a type-check cannot
// catch:
//
//   - every chat surface draws its bubble through one shared row, so the
//     direct, room, channel and discussion shapes cannot drift apart again;
//   - the row carries the bubble's width cap and its sender-side alignment
//     and nothing else -- a second cap or a second alignment is how a bubble
//     ends up wider than the transcript allows, or on the wrong side;
//   - the row holds the hover scope the action row's reveal keys off, or
//     the buttons never show to a hovering pointer;
//   - that bubble gets the raw toggle's pin, or the swap moves the viewport
//     the reader was standing on.
//
// The surface table below is the evidence that no shared row component has
// been pulled out yet: three surfaces each spell their row container out by
// hand, and one of them spelled it without the hover scope. It is not the
// destination -- it holds "a surface is missing", not "a surface wrote
// something that only looks like the row" -- but it is what makes the next
// surface announce itself instead of sliding past every loop below.

import { assert, assertEquals } from "@std/assert";

import { flat } from "../test_support/flat.ts";
import { source as readSource } from "../lib/testing/source_pins.ts";
import { transcriptSurfaces } from "../lib/testing/transcript_surfaces.ts";

const ROW = new URL("./MessageRow.svelte", import.meta.url);
const VIEW = new URL("./ConversationView.svelte", import.meta.url);
const ROOM = new URL("../pages/RoomConversationPage.svelte", import.meta.url);
const THREAD = new URL("./rooms/ThreadConversation.svelte", import.meta.url);

/** Every surface that draws a bubble, with the name its failure reads. */
const SURFACES = [
  ["direct and channel", VIEW],
  ["room", ROOM],
  ["discussion", THREAD],
] as const;

/** The surface's source, comments and formatting flattened away. */
function source(url: URL): string {
  return flat(readSource(url));
}

/** The opening tag of the element that contains `at`. */
function openingTagOf(src: string, at: number): string {
  const open = src.lastIndexOf("<div", at);
  assert(open >= 0, "the bubble is not inside an element");
  const close = src.indexOf(">", open);
  assert(close > open, "that element never closes its opening tag");
  return src.slice(open, close + 1);
}

/** The opening tag of the shared row a surface draws its bubbles through. */
function rowTagOf(src: string): string {
  const row = src.indexOf("<MessageRow");
  assert(row >= 0, "the surface never draws the shared row");
  return openingTagOf(src, row);
}

/** The class attribute's own words, not the letters `group` anywhere in it. */
function classTokensOf(tag: string): string[] {
  const attribute = /\bclass="([^"]*)"/u.exec(tag);
  assert(attribute, "the element carries a class");
  return attribute[1].split(/\s+/u);
}

/** The opening tag the prop sits in. */
function tagAround(src: string, prop: RegExp): string {
  const found = prop.exec(src);
  assert(found, `the surface passes ${prop}`);
  const at = found.index;
  const open = src.lastIndexOf("<", at);
  assert(open >= 0, "that prop sits in an element");
  const close = src.indexOf(">", at);
  assert(close > at, "that element never closes its opening tag");
  return src.slice(open, close + 1);
}

// A row container is not what makes a surface: the file has to be one that
// draws a bubble, so the table is checked against the files that import the
// shared row. A fourth surface turns this red instead of slipping past.
Deno.test("the surface table is the files that draw the shared row", () => {
  assertEquals(
    transcriptSurfaces()
      .map((surface) => surface.url.href)
      .sort(),
    SURFACES.map(([, url]) => url.href).sort(),
    "a file that draws the shared row is missing from the surface table",
  );
});

Deno.test("every chat surface draws its bubble through the shared row", () => {
  for (const [label, url] of SURFACES) {
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

// The action row's reveal is `group-hover`: under a hovering pointer the three
// buttons sit at opacity 0 until the bubble's row is hovered, and the shared
// segment keys off an ancestor carrying `.group` -- a button brings none of
// its own. A surface whose row container lacks the scope keeps its buttons for
// the keyboard alone, which is exactly how the discussion drifted from the
// chat page. The token is read from the class attribute's own words: a
// `role="group"` in the same tag, or a `group-hover:` neighbour, spells the
// letters and grants no scope at all.
Deno.test("every surface gives the hover reveal a row to fire on", () => {
  for (const [label, url] of SURFACES) {
    const tokens = classTokensOf(rowTagOf(source(url)));
    assert(
      tokens.includes("group"),
      `${label} row container carries the hover scope`,
    );
  }
});

// Both halves of the wiring are matched with their whitespace open: a formatter
// that wraps the factory call or the prop's braces must not decide the pin.
const PIN_FACTORY =
  /createTogglePin\(\s*\(\)\s*=>\s*scroll\.viewport\s*,?\s*\)/u;
const PIN_PROP = /pin=\{[^}]*togglePin/u;

// The raw toggle swaps the bubble for its source and the row changes height;
// the pin re-anchors the viewport on that row for a moment afterwards, so the
// reader stays where they were reading. A type-check would stop a pin given
// to the shared row, which takes no such prop; what it cannot see is the pin
// that is missing altogether, or one handed to another bubble inside the
// same row, where the surface would still pass both checks.
Deno.test(
  "every surface pins the bubble against the raw toggle's resize",
  () => {
    for (const [label, url] of SURFACES) {
      const src = source(url);
      assert(
        PIN_FACTORY.test(src),
        `${label} builds the pin on its own viewport`,
      );
      assert(
        tagAround(src, PIN_PROP).startsWith("<MessageBubble"),
        `${label} hand that pin to the bubble`,
      );
    }
  },
);
