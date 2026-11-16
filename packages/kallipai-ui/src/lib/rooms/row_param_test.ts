// A row number from a URL: the one rule the room and discussion pages share.
// A reading that accepts "0" or "7abc" opens a discussion no page can read,
// so the boundary is what these cases pin.

import { assertEquals } from "@std/assert";
import { rowParamOf } from "./rowParam.ts";

Deno.test("a row number is a whole number from 1", () => {
  assertEquals(rowParamOf("1"), 1);
  assertEquals(rowParamOf("12"), 12);
  assertEquals(rowParamOf("9007199254740991"), Number.MAX_SAFE_INTEGER);
});

Deno.test("a value that names no row reads as none", () => {
  for (const value of [
    undefined,
    "",
    "0",
    "007",
    "-3",
    "+3",
    "3.5",
    "7abc",
    " 7",
    "7 ",
    "1e3",
    "0x10",
    "9007199254740992",
  ]) {
    assertEquals(rowParamOf(value), null, `value: ${String(value)}`);
  }
});
