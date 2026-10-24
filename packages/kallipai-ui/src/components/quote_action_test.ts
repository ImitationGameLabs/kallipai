// Two source-read pins. The first fixes the only shape the surfaces may keep:
// a quotable row offers the quote control inside the bubble's own action row,
// once, and nothing else draws it. A type-check cannot see a second placement,
// or a surface that stopped handing the row something to render. The second
// pins the one reveal segment the three action buttons share.

import { assert, assertEquals } from "@std/assert";

const BUBBLE = new URL("./MessageBubble.svelte", import.meta.url);
const VIEW = new URL("./ConversationView.svelte", import.meta.url);
const ROOM = new URL("../pages/RoomConversationPage.svelte", import.meta.url);

function source(url: URL): string {
  return new TextDecoder().decode(Deno.readFileSync(url));
}

Deno.test(
  "a quotable row offers the quote control in the action row, once",
  () => {
    const bubble = source(BUBBLE);
    const guard = bubble.indexOf("{#if !bare}");
    assert(guard >= 0, "the action row renders under the non-bare guard");
    const row = bubble.slice(guard);
    const slot = row.indexOf("{@render extraActions?.()}");
    const copy = row.indexOf("<CopyButton");
    assert(
      slot >= 0 && copy > slot,
      "the surface's action leads the row, ahead of the copy control",
    );
    assert(
      !bubble.includes("<QuoteButton"),
      "the bubble itself draws no quote control",
    );

    for (const [label, url] of [
      ["direct and channel", VIEW],
      ["room", ROOM],
    ] as const) {
      const src = source(url);
      const open = src.indexOf("{#snippet extraActions()}");
      const close = src.indexOf("{/snippet}", open);
      assert(open >= 0 && close > open, `${label} declare the action snippet`);
      const snippet = src.slice(open, close);
      assert(
        snippet.includes("<QuoteButton"),
        `${label} put the quote control in that snippet`,
      );
      assertEquals(
        src.split("<QuoteButton").length - 1,
        1,
        `${label} draw the quote control once`,
      );
      assert(
        snippet.includes("{#if quotable(line)}"),
        `${label} offer it only on a row it can quote`,
      );
      assertEquals(
        src.split("{extraActions}").length - 1,
        1,
        `${label} hand that one snippet to the bubble`,
      );
      if (url === VIEW) {
        assert(
          snippet.includes("onclick={() => onQuote?.(line)}"),
          `${label} wire the click to the surface's quote handler`,
        );
      }
    }
  },
);

// The three action buttons reveal themselves through one class segment: the
// reveal is written in the class module, so a fourth button cannot arrive with
// a reveal of its own, nor can one of the three drift to a hand-written copy.
Deno.test("every action button reveals through the one shared segment", () => {
  const segment = "../lib/classes.ts";
  const shared = source(new URL(segment, import.meta.url));
  assert(
    shared.includes("export const BUBBLE_ACTION"),
    "the class module exports the segment",
  );
  for (const label of ["CopyButton", "RawToggleButton", "QuoteButton"]) {
    const src = source(new URL(`./${label}.svelte`, import.meta.url));
    assert(src.includes("{BUBBLE_ACTION}"), `${label} wears the segment`);
  }
  for (const entry of Deno.readDirSync(new URL(".", import.meta.url))) {
    if (!entry.name.endsWith(".svelte")) continue;
    const src = source(new URL(`./${entry.name}`, import.meta.url));
    assert(
      !src.includes("[@media(hover:hover)]:opacity-0"),
      `${entry.name} writes no reveal of its own`,
    );
  }
});
