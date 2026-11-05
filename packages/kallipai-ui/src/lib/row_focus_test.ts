// Which row the transcript lands on after a discard. The order handed in is the
// one that REMAINS; the second argument is the place the departed row held in
// the order that was rendered. Reading it afterwards by id cannot work: the
// store drops the line as it answers, so the id is already gone and a lookup
// answers with whatever sits first.

import { assertEquals } from "@std/assert";
import { neighbourRow } from "./row_focus.ts";

Deno.test("neighbourRow prefers the row above the one that left", () => {
  assertEquals(neighbourRow([-1, -3], 1), -1);
});

Deno.test(
  "neighbourRow falls to the row that took over the first place",
  () => {
    assertEquals(neighbourRow([-2, -3], 0), -2);
  },
);

Deno.test("neighbourRow reads the index, not an id lookup", () => {
  // The row at the END left: the order that remains is [-1, -2], the place it
  // held is 2. Seeking it by id in what remains finds nothing and the first
  // row is not the answer; the place is.
  assertEquals(neighbourRow([-1, -2], 2), -2);
});

Deno.test("neighbourRow reads render order, not id order", () => {
  // Optimistic seqs are handed out downward, so id order runs against the
  // rendered order; the answer must follow the transcript.
  assertEquals(neighbourRow([-4, -2, -3], 1), -4);
});

Deno.test("neighbourRow is null when nothing is left to land on", () => {
  assertEquals(neighbourRow([], 0), null);
});

Deno.test("a place outside the remaining order is refused", () => {
  // A stale index describes a longer transcript than the one handed in, and
  // answering it with the first row is how a reader ends up at the top.
  assertEquals(neighbourRow([-1, -2], 3), null);
  assertEquals(neighbourRow([-1, -2], -1), null);
});
