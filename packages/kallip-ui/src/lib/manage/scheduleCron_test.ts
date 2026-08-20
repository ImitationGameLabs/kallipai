// Behavior tests for the schedule-cron subset translation: what parses,
// what falls back, and the boundary semantics the card summary and the
// next-start hint rely on.

import { assertEquals } from "@std/assert";
import {
  compileSubset,
  describeCron,
  nextStart,
  parseSubset,
} from "./scheduleCron.ts";

Deno.test("parseSubset: accepts canonical weekday expression", () => {
  const form = parseSubset("0 9 * * 1-5");
  assertEquals(form, { minute: 0, hour: 9, dows: [1, 2, 3, 4, 5] });
});

Deno.test("parseSubset: accepts day list and sorts + dedupes it", () => {
  const form = parseSubset("30 17 * * 5,1,5,0");
  assertEquals(form, { minute: 30, hour: 17, dows: [0, 1, 5] });
});

Deno.test("parseSubset: rejects expressions outside the subset", () => {
  // step hour, dom part, month part, name dow, six fields, empty.
  assertEquals(parseSubset("0 */2 * * 1"), null);
  assertEquals(parseSubset("0 9 1 * 1"), null);
  assertEquals(parseSubset("0 9 * 2 1"), null);
  assertEquals(parseSubset("0 9 * * mon"), null);
  assertEquals(parseSubset("0 9 * * 1-5 extra"), null);
  assertEquals(parseSubset(""), null);
});

Deno.test("compileSubset: canonical string from any dow order", () => {
  assertEquals(
    compileSubset({ minute: 0, hour: 9, dows: [5, 1, 3] }),
    "0 9 * * 1,3,5",
  );
});

Deno.test("compileSubset: single day compiles without list separator", () => {
  assertEquals(
    compileSubset({ minute: 59, hour: 23, dows: [6] }),
    "59 23 * * 6",
  );
});

Deno.test("parse and compile round-trip through the canonical form", () => {
  const cron = "15 8 * * 0,2,4";
  assertEquals(compileSubset(parseSubset(cron)!), cron);
});

Deno.test("describeCron: merges one line when both crons share days", () => {
  const s = describeCron("0 9 * * 1-5", "0 17 * * 1,2,3,4,5");
  assertEquals(s, "weekdays 09:00–17:00 (UTC)");
});

Deno.test("compileSubset: every day compiles to the star form", () => {
  assertEquals(
    compileSubset({ minute: 0, hour: 22, dows: [0, 1, 2, 3, 4, 5, 6] }),
    "0 22 * * *",
  );
});
Deno.test("parseSubset: star dow parses as every day", () => {
  assertEquals(parseSubset("0 22 * * *")?.dows.length, 7);
});

Deno.test("describeCron: overnight window notes the next day", () => {
  const s = describeCron("0 22 * * *", "0 6 * * *");
  assertEquals(s, "every day 22:00–next day 06:00 (UTC)");
});

Deno.test("describeCron: same-hour overnight window still summarizes", () => {
  // A same-hour wrap (09:50 start, 09:10 end) is overnight, not zero-
  // width; only exact equality falls back to the raw cron.
  const s = describeCron("50 9 * * *", "10 9 * * *");
  assertEquals(s, "every day 09:50–next day 09:10 (UTC)");
});

Deno.test("describeCron: split days render start and end day sets", () => {
  const s = describeCron("0 9 * * 1-5", "0 18 * * 6,0");
  assertEquals(s, "weekdays 09:00 – weekends 18:00 (UTC)");
});

Deno.test("describeCron: zero-width window falls back to raw cron", () => {
  // end == start never opens a window (engine compares strictly); there
  // is no honest plain-form summary, so the caller renders verbatim.
  assertEquals(describeCron("0 9 * * 1-5", "0 9 * * 1-5"), null);
});

Deno.test("describeCron: outside subset falls back to raw cron", () => {
  assertEquals(describeCron("0 */2 * * *", "0 8 * * *"), null);
});

Deno.test("nextStart: later today when the time has not passed", () => {
  const now = new Date("2026-08-24T06:00:00Z"); // Monday
  const next = nextStart("0 9 * * 1-5", now);
  assertEquals(next?.toISOString(), "2026-08-24T09:00:00.000Z");
});

Deno.test("nextStart: skips to the next matching weekday", () => {
  const now = new Date("2026-08-24T10:00:00Z"); // Monday, past 09:00
  const next = nextStart("0 9 * * 1-5", now);
  assertEquals(next?.toISOString(), "2026-08-25T09:00:00.000Z");
});

Deno.test("nextStart: wraps to next week when the week is spent", () => {
  const now = new Date("2026-08-28T10:00:00Z"); // Friday, past 09:00
  const next = nextStart("0 9 * * 1-5", now);
  assertEquals(next?.toISOString(), "2026-08-31T09:00:00.000Z"); // Monday
});

Deno.test("nextStart: strict inequality at the exact fire time", () => {
  const now = new Date("2026-08-24T09:00:00Z"); // exactly at the fire time
  const next = nextStart("0 9 * * 1-5", now);
  assertEquals(next?.toISOString(), "2026-08-25T09:00:00.000Z");
});

Deno.test("nextStart: null outside the subset", () => {
  assertEquals(nextStart("0 */2 * * *", new Date()), null);
});
