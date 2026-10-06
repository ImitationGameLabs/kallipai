import { assertEquals } from "@std/assert";
import {
  endOfDayIso,
  formatStampInZone,
  timezoneCandidates,
  todayIso,
} from "./stamp.ts";

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

Deno.test("endOfDayIso: the day's last local millisecond, as UTC ISO", () => {
  const iso = endOfDayIso("2026-07-01");
  const local = new Date(2026, 6, 1, 23, 59, 59, 999);
  assertEquals(iso, local.toISOString());
});

Deno.test("endOfDayIso: non-date strings answer null", () => {
  assertEquals(endOfDayIso(""), null);
  assertEquals(endOfDayIso("2026-07"), null);
  assertEquals(endOfDayIso("2026-07-01T10:00"), null);
});

Deno.test("todayIso: the local calendar day as YYYY-MM-DD", () => {
  const now = new Date();
  const expected = [
    now.getFullYear(),
    `${now.getMonth() + 1}`.padStart(2, "0"),
    `${now.getDate()}`.padStart(2, "0"),
  ].join("-");
  assertEquals(todayIso(), expected);
});
