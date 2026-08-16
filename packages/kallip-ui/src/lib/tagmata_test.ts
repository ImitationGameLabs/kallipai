import { assertEquals } from "@std/assert";
import { formatRemaining, formatTagmaStatusLine } from "./tagmata.svelte.ts";

Deno.test("formatRemaining: zero or negative -> expired", () => {
  assertEquals(formatRemaining(0), "expired");
  assertEquals(formatRemaining(-1), "expired");
});

Deno.test("formatRemaining: sub-minute -> <1min", () => {
  assertEquals(formatRemaining(1), "<1min");
  assertEquals(formatRemaining(59_999), "<1min");
});

Deno.test(
  "formatTagmaStatusLine: en renders active/total and token counts",
  () => {
    // char-exact: the message migration must not change the en readout.
    assertEquals(
      formatTagmaStatusLine({
        rootState: "busy",
        subagentsTotal: 3,
        subagentsActive: 1,
        tokenBudget: 50_000,
        tokenConsumed: 12_345,
      }),
      "2/4 agents · 12.3k/50k tokens",
    );
  },
);

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
