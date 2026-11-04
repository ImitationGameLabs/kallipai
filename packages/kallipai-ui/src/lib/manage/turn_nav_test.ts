// The walk rules for the turn list: one row per arrow, the ends hold, and a
// list with no row under the focus starts at its top.

import { assertEquals } from "@std/assert";
import { stepIndex } from "./turn_nav.ts";

Deno.test("an arrow moves one row and the ends hold", () => {
  assertEquals(stepIndex(0, 3, "ArrowDown"), 1);
  assertEquals(stepIndex(1, 3, "ArrowUp"), 0);
  assertEquals(stepIndex(2, 3, "ArrowDown"), 2);
  assertEquals(stepIndex(0, 3, "ArrowUp"), 0);
});

Deno.test("Home and End reach the first and last row", () => {
  assertEquals(stepIndex(1, 5, "Home"), 0);
  assertEquals(stepIndex(1, 5, "End"), 4);
  assertEquals(stepIndex(4, 1, "End"), 0);
});

Deno.test("no row under the focus sends either arrow to the first row", () => {
  assertEquals(stepIndex(-1, 4, "ArrowDown"), 0);
  assertEquals(stepIndex(-1, 4, "ArrowUp"), 0);
});

Deno.test("an empty list and keys about something else are left alone", () => {
  assertEquals(stepIndex(0, 0, "ArrowDown"), null);
  assertEquals(stepIndex(-1, 0, "ArrowUp"), null);
  assertEquals(stepIndex(1, 3, "Enter"), null);
  assertEquals(stepIndex(1, 3, "Tab"), null);
  assertEquals(stepIndex(1, 3, "a"), null);
});

Deno.test("an index past an end is clamped to that end first", () => {
  assertEquals(stepIndex(9, 3, "ArrowDown"), 2);
  assertEquals(stepIndex(9, 3, "ArrowUp"), 1);
  assertEquals(stepIndex(3, 3, "ArrowDown"), 2);
  assertEquals(stepIndex(-5, 3, "ArrowDown"), 0);
  assertEquals(stepIndex(-5, 3, "ArrowUp"), 0);
});
