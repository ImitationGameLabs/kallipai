import { assertEquals } from "@std/assert";
import { formatRemaining } from "./tagmata.svelte.ts";

Deno.test("formatRemaining: zero or negative -> expired", () => {
  assertEquals(formatRemaining(0), "expired");
  assertEquals(formatRemaining(-1), "expired");
});

Deno.test("formatRemaining: sub-minute -> <1min", () => {
  assertEquals(formatRemaining(1), "<1min");
  assertEquals(formatRemaining(59_999), "<1min");
});

Deno.test("formatRemaining: drops leading zero units", () => {
  // 3 minutes exactly.
  assertEquals(formatRemaining(3 * 60_000), "3min");
  // 2h 3min (no days).
  assertEquals(formatRemaining(2 * 3_600_000 + 3 * 60_000), "2h 3min");
});

Deno.test("formatRemaining: full days/hours/minutes", () => {
  const ms = 1 * 86_400_000 + 2 * 3_600_000 + 3 * 60_000;
  assertEquals(formatRemaining(ms), "1d 2h 3min");
});

Deno.test("formatRemaining: days and minutes with zero hours", () => {
  // 1d 0h 3min -> hours omitted.
  const ms = 1 * 86_400_000 + 3 * 60_000;
  assertEquals(formatRemaining(ms), "1d 3min");
});
