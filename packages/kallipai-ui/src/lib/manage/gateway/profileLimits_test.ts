import { assertEquals } from "@std/assert";
import { parseProfileLimit } from "./profileLimits.ts";

Deno.test("blank is no limit, a number is itself", () => {
  assertEquals(parseProfileLimit(""), null);
  assertEquals(parseProfileLimit("   "), null);
  assertEquals(parseProfileLimit("64000"), 64_000);
  assertEquals(parseProfileLimit(" 64000 "), 64_000);
  assertEquals(parseProfileLimit("0"), 0);
});

Deno.test("unreadable text answers undefined, never a number", () => {
  for (const raw of [
    "abc",
    "12abc",
    "Infinity",
    "-Infinity",
    "NaN",
    "1,000",
    "1e999",
  ]) {
    assertEquals(parseProfileLimit(raw), undefined, raw);
  }
});

// The reading stays `Number`'s, as both dialogs had it: anything it reads as
// finite passes, so `1e3` and `0x10` do too. The sibling ../gateway/setDraft.ts
// parseContextWindow is stricter about the same max_context_window field (a
// positive whole number), so the two faces still disagree; one rule for both
// is a separate change.
Deno.test("a loose reading keeps 1e3 and 0x10 submittable", () => {
  for (const raw of ["1.5", "1e3", "-5", "0x10", "+5", ".5", "1e308"]) {
    assertEquals(parseProfileLimit(raw), Number(raw), raw);
  }
});
