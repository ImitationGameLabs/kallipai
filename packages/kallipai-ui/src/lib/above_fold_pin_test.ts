// What a growth above the reader costs them: the rule that decides how much of
// it to hand back. The growth lands on the row's bottom edge; if that point sat
// at or above the viewport's top edge, every row the reader is looking at moved
// down by the whole gain, and all of it comes back. Otherwise nothing is owed.

import { assertEquals } from "@std/assert";
import { aboveFoldShift } from "./transcript.svelte.ts";

Deno.test("a row that grew above the viewport owes its height back", () => {
  // The card ends above the fold (it grew to 100 from 78 while its top sat at
  // -60, viewport top 100): everything below it moved down by 22.
  assertEquals(aboveFoldShift({ top: -60, height: 100 }, 100, 78), 22);
  // Far above it, the distance to the edge stops mattering: the gain is what
  // moved the reader's rows, however large it is.
  assertEquals(aboveFoldShift({ top: -500, height: 600 }, 100, 100), 500);
});

Deno.test("a row the reader can see keeps its growth", () => {
  assertEquals(aboveFoldShift({ top: 120, height: 120 }, 100, 98), 0);
  // The same row touching the top edge is still on screen.
  assertEquals(aboveFoldShift({ top: 100, height: 120 }, 100, 98), 0);
});

Deno.test("a row that did not change owes nothing", () => {
  assertEquals(aboveFoldShift({ top: -400, height: 100 }, 100, 100), 0);
});

Deno.test("scrolling is not growth", () => {
  // The reader scrolled 900px between two observations: the row's top moved,
  // its height did not, and nothing is owed back.
  assertEquals(aboveFoldShift({ top: -960, height: 100 }, 100, 100), 0);
});

Deno.test("a row that shrank is not chased upward", () => {
  assertEquals(aboveFoldShift({ top: -60, height: 78 }, 100, 100), 0);
});

Deno.test(
  "a row whose bottom still sits below the fold keeps its growth",
  () => {
    // The bubble straddles the edge: top 50, viewport top 100, height 120 from
    // 98. Its bottom was 148, under the edge, so nothing of the gain moved a
    // line the reader was looking at.
    assertEquals(aboveFoldShift({ top: 50, height: 120 }, 100, 98), 0);
  },
);

Deno.test("a row wholly above the fold hands it all back", () => {
  // Top 0, height 180 from 80: the bottom sat 80, above the edge at 100, so the
  // 100 of new height pushed every row below the edge down by 100.
  assertEquals(aboveFoldShift({ top: 0, height: 180 }, 100, 80), 100);
});

Deno.test("a bottom edge on the fold counts as above it", () => {
  // Top 10, height 100 from 90: the bottom sat at exactly 100, so the row held
  // no visible pixel and its gain of 10 moved the reader's rows by all of it.
  assertEquals(aboveFoldShift({ top: 10, height: 100 }, 100, 90), 10);
});

Deno.test("a bottom edge one pixel under the fold owes nothing", () => {
  // The same row one pixel lower. It has a line of its own on screen, and the
  // gain landed under the edge, in front of the reader.
  assertEquals(aboveFoldShift({ top: 11, height: 101 }, 100, 90), 0);
});
