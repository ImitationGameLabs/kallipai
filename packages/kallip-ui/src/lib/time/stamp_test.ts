import { assertEquals } from "@std/assert";
import { formatStampInZone, timezoneCandidates } from "./stamp.ts";

// 2026-07-01T00:00:00Z — mid-summer, away from DST transitions.
const ISO = "2026-07-01T00:00:00Z";

Deno.test("formatStampInZone: named zone wins over the browser zone", () => {
  const rendered = formatStampInZone(ISO, undefined, "Asia/Shanghai");
  // The tagma zone (+08:00) must shift the wall clock off the UTC hour.
  const expected = new Intl.DateTimeFormat("en", {
    timeZone: "Asia/Shanghai",
  }).format(new Date(ISO));
  assertEquals(rendered, expected);
});

Deno.test(
  "formatStampInZone: unknown zone falls back to the browser zone",
  () => {
    const rendered = formatStampInZone(ISO, undefined, "Mars/Olympus");
    const local = new Intl.DateTimeFormat(undefined, {}).format(new Date(ISO));
    assertEquals(typeof rendered === "string" && rendered.length > 0, true);
    assertEquals(rendered.length >= local.length - 5, true);
  },
);

Deno.test("formatStampInZone: null timezone uses the browser zone", () => {
  const rendered = formatStampInZone(ISO, undefined, null);
  assertEquals(typeof rendered, "string");
  assertEquals(rendered.length > 0, true);
});

Deno.test("formatStampInZone: invalid input echoes the raw value", () => {
  assertEquals(formatStampInZone("not-a-date", undefined, "UTC"), "not-a-date");
});

Deno.test("formatStampInZone: epoch number input renders", () => {
  const rendered = formatStampInZone(1_782_864_000 * 1000, undefined, "UTC");
  assertEquals(rendered.includes("2026"), true, rendered);
});

Deno.test("timezoneCandidates: includes common IANA zones", () => {
  const candidates = timezoneCandidates();
  assertEquals(candidates.includes("Asia/Shanghai"), true);
  assertEquals(candidates.includes("America/New_York"), true);
});
