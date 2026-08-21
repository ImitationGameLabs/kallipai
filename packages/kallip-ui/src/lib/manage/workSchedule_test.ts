// Tests for workSchedule.ts: validation, minute/offset math, and the
// evaluator port. Evaluator cases mirror the Rust eval.rs matrix so the
// client preview and the backend cannot drift apart silently.

import { assertEquals } from "@std/assert";
import {
  DAY_MINUTES,
  hhmmToMinute,
  localOffsetMinutes,
  minuteToHHMM,
  monthDayList,
  offsetLabel,
  shiftMinute,
  validateSpec,
  weekdayList,
  windowStatus,
} from "./workSchedule.ts";
import type { WorkScheduleSpec } from "@kallipai/kallip-client";

const utc = (y: number, mo: number, d: number, h = 0, mi = 0) =>
  new Date(Date.UTC(y, mo - 1, d, h, mi));

// --- validation ---

Deno.test("validateSpec: weekly happy path", () => {
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b0011_1111,
    start_minute: 540,
    end_minute: 1020,
  };
  assertEquals(validateSpec(spec), null);
});

Deno.test("validateSpec: empty day mask rejected in both modes", () => {
  assertEquals(
    validateSpec({ mode: "weekly", days: 0, start_minute: 0, end_minute: 60 }),
    "days_empty",
  );
  assertEquals(
    validateSpec({ mode: "monthly", days: 0, start_minute: 0, end_minute: 60 }),
    "days_empty",
  );
});

Deno.test("validateSpec: weekly mask limited to bits 0-6", () => {
  assertEquals(
    validateSpec({
      mode: "weekly",
      days: 1 << 7,
      start_minute: 0,
      end_minute: 60,
    }),
    "days_range",
  );
});

Deno.test("validateSpec: zero window never opens", () => {
  assertEquals(
    validateSpec({
      mode: "weekly",
      days: 1,
      start_minute: 600,
      end_minute: 600,
    }),
    "zero_window",
  );
});

Deno.test("validateSpec: end 1440 is a legal full-day window", () => {
  assertEquals(
    validateSpec({
      mode: "weekly",
      days: 1,
      start_minute: 0,
      end_minute: 1440,
    }),
    null,
  );
});

Deno.test("validateSpec: window bounds mirror the backend", () => {
  // spec.rs rejects end_minute == 0 and start_minute == 1440; the UI must
  // fail the same specs before the PUT instead of surfacing a raw 400.
  assertEquals(
    validateSpec({
      mode: "weekly",
      days: 1,
      start_minute: 600,
      end_minute: 0,
    }),
    "minute_range",
  );
  assertEquals(
    validateSpec({
      mode: "monthly",
      days: 1,
      start_minute: 1440,
      end_minute: 100,
    }),
    "minute_range",
  );
});

Deno.test("validateSpec: interval ranges", () => {
  assertEquals(
    validateSpec({
      mode: "interval",
      every_hours: 0,
      length_min: 30,
      anchor: "2026-08-21T12:00:00Z",
    }),
    "every_hours_range",
  );
  assertEquals(
    validateSpec({
      mode: "interval",
      every_hours: 5,
      length_min: 0,
      anchor: "2026-08-21T12:00:00Z",
    }),
    "length_min_range",
  );
  assertEquals(
    validateSpec({
      mode: "interval",
      every_hours: 5,
      length_min: 7 * DAY_MINUTES + 1,
      anchor: "2026-08-21T12:00:00Z",
    }),
    "length_min_range",
  );
  assertEquals(
    validateSpec({
      mode: "interval",
      every_hours: 5,
      length_min: 7 * DAY_MINUTES,
      anchor: "2026-08-21T12:00:00Z",
    }),
    null,
  );
});

// --- minute and offset math ---

Deno.test("minuteToHHMM/hhmmToMinute round trip", () => {
  assertEquals(minuteToHHMM(0), "00:00");
  assertEquals(minuteToHHMM(540), "09:00");
  assertEquals(minuteToHHMM(1439), "23:59");
  assertEquals(hhmmToMinute("09:00"), 540);
  assertEquals(hhmmToMinute("9:00"), 540);
  assertEquals(hhmmToMinute("24:01"), null);
  assertEquals(hhmmToMinute("09:60"), null);
  assertEquals(hhmmToMinute("9am"), null);
});

Deno.test(
  "shiftMinute wraps in both directions (half-hour offsets too)",
  () => {
    // +08:00 east: 23:00 UTC is 07:00 next day local.
    assertEquals(shiftMinute(23 * 60, 8 * 60), 7 * 60);
    // -05:00 west: 02:00 UTC is 21:00 previous day local.
    assertEquals(shiftMinute(2 * 60, -5 * 60), 21 * 60);
    // Half-hour zone (UTC+05:30): 23:45 UTC → 05:15 local.
    assertEquals(shiftMinute(23 * 60 + 45, 5 * 60 + 30), 5 * 60 + 15);
    assertEquals(shiftMinute(0, 0), 0);
  },
);

Deno.test("offsetLabel renders signs and half hours", () => {
  assertEquals(offsetLabel(8 * 60), "UTC+08:00");
  assertEquals(offsetLabel(-5 * 60), "UTC-05:00");
  assertEquals(offsetLabel(5 * 60 + 30), "UTC+05:30");
  assertEquals(offsetLabel(0), "UTC+00:00");
});

Deno.test("localOffsetMinutes matches the Date API", () => {
  const d = new Date("2026-08-21T12:00:00Z");
  assertEquals(localOffsetMinutes(d), -d.getTimezoneOffset());
});

// --- masks ---

Deno.test("weekdayList is Monday-first ISO order", () => {
  // bit 0 = Mon, bit 6 = Sun: Sun+Mon+Wed → [1, 3, 7].
  assertEquals(weekdayList((1 << 0) | (1 << 2) | (1 << 6)), [1, 3, 7]);
});

Deno.test("monthDayList enumerates selected days", () => {
  // The 1st, the 15th, the 31st.
  assertEquals(monthDayList((1 << 0) | (1 << 14) | (1 << 30)), [1, 15, 31]);
});

// --- evaluator: weekly ---

Deno.test("weekly inside and half-open boundary semantics", () => {
  // Mon Aug 10 2026; Mon 09:00-17:00.
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b0000_0001,
    start_minute: 540,
    end_minute: 1020,
  };
  const st = windowStatus(spec, utc(2026, 8, 10, 12))!;
  assertEquals(st.inside, true);
  assertEquals(st.nextEnd.getTime(), utc(2026, 8, 10, 17).getTime());
  assertEquals(st.nextStart.getTime(), utc(2026, 8, 17, 9).getTime());
  // The end instant itself is already outside ([start, end)).
  assertEquals(windowStatus(spec, utc(2026, 8, 10, 17))!.inside, false);
  // The start instant is inside.
  assertEquals(windowStatus(spec, utc(2026, 8, 10, 9))!.inside, true);
});

Deno.test("weekly overnight window belongs to its start day", () => {
  // Mon 22:00 → Tue 06:00. Tuesday 02:00 is inside (Monday's window).
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b0000_0001,
    start_minute: 22 * 60,
    end_minute: 6 * 60,
  };
  const st = windowStatus(spec, utc(2026, 8, 11, 2))!;
  assertEquals(st.inside, true);
  assertEquals(st.nextEnd.getTime(), utc(2026, 8, 11, 6).getTime());
});

Deno.test("weekly overnight covers the exact midnight boundary", () => {
  // Mirrors eval.rs weekly_overnight_covers_midnight_boundary: Monday's
  // 22:00 → 06:00 window still owns the instant 00:00 itself.
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b0000_0001,
    start_minute: 22 * 60,
    end_minute: 6 * 60,
  };
  const st = windowStatus(spec, utc(2026, 8, 11, 0))!;
  assertEquals(st.inside, true);
});

Deno.test("weekly next start after a far gap", () => {
  // Sunday-only 10:00-12:00; ask on Monday.
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b0100_0000,
    start_minute: 600,
    end_minute: 720,
  };
  const st = windowStatus(spec, utc(2026, 8, 10, 0))!;
  assertEquals(st.inside, false);
  assertEquals(st.nextStart.getTime(), utc(2026, 8, 16, 10).getTime());
});

Deno.test("weekly all days means daily", () => {
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b0111_1111,
    start_minute: 540,
    end_minute: 1020,
  };
  assertEquals(windowStatus(spec, utc(2026, 8, 15, 10))!.inside, true);
  assertEquals(windowStatus(spec, utc(2026, 8, 16, 10))!.inside, true);
});

// --- evaluator: monthly ---

Deno.test("monthly skips days absent from short months", () => {
  // The 30th and 31st only; February 2027 has neither (28 days).
  const spec: WorkScheduleSpec = {
    mode: "monthly",
    days: (1 << 29) | (1 << 30),
    start_minute: 540,
    end_minute: 1020,
  };
  const st = windowStatus(spec, utc(2027, 2, 15, 0))!;
  assertEquals(st.inside, false);
  // Next fire is Mar 30, not Feb 30/31.
  assertEquals(st.nextStart.getTime(), utc(2027, 3, 30, 9).getTime());
});

Deno.test("monthly feb 29 leap years", () => {
  const spec: WorkScheduleSpec = {
    mode: "monthly",
    days: 1 << 28,
    start_minute: 0,
    end_minute: 60,
  };
  // Feb 2028 has 29 days: fires.
  assertEquals(windowStatus(spec, utc(2028, 2, 29, 0))!.inside, true);
  // Feb 2027 does not have a 29th: the next fire is Mar 29 2027.
  const st = windowStatus(spec, utc(2027, 2, 28, 1))!;
  assertEquals(st.nextStart.getTime(), utc(2027, 3, 29, 0).getTime());
});

Deno.test("monthly cross-month overnight window", () => {
  // The 31st 23:00 → 01:00 next day; ask on Sep 1 00:30 (Aug 31's window).
  const spec: WorkScheduleSpec = {
    mode: "monthly",
    days: 1 << 30,
    start_minute: 23 * 60,
    end_minute: 60,
  };
  const st = windowStatus(spec, utc(2026, 9, 1, 0, 30))!;
  assertEquals(st.inside, true);
  assertEquals(st.nextEnd.getTime(), utc(2026, 9, 1, 1).getTime());
});

Deno.test("full-day window via end_minute 1440", () => {
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 1,
    start_minute: 0,
    end_minute: DAY_MINUTES,
  };
  assertEquals(windowStatus(spec, utc(2026, 8, 10, 0))!.inside, true);
  assertEquals(windowStatus(spec, utc(2026, 8, 10, 23, 59))!.inside, true);
});

Deno.test("adjacent windows merge covering end", () => {
  // Every day 22:00→06:00 plus the same days 04:00→08:00: a Tuesday 05:00
  // now is covered through Tue 08:00, not just 06:00.
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b0111_1111,
    start_minute: 22 * 60,
    end_minute: 8 * 60, // 22:00 → 08:00 already spans both
  };
  const st = windowStatus(spec, utc(2026, 8, 11, 5))!;
  assertEquals(st.inside, true);
  assertEquals(st.nextEnd.getTime(), utc(2026, 8, 11, 8).getTime());
});

// --- evaluator: interval ---

Deno.test("interval strict rotation across day boundaries", () => {
  // Every 5h, 90min, anchored Aug 21 17:00 UTC. Shifts: 17:00, 22:00,
  // next-day 03:00 — the gap stays 5h across midnight.
  const spec: WorkScheduleSpec = {
    mode: "interval",
    every_hours: 5,
    length_min: 90,
    anchor: "2026-08-21T17:00:00Z",
  };
  const st = windowStatus(spec, utc(2026, 8, 21, 18))!;
  assertEquals(st.inside, true);
  assertEquals(st.nextEnd.getTime(), utc(2026, 8, 21, 18, 30).getTime());
  // Between shifts at 20:00: next is 22:00.
  const between = windowStatus(spec, utc(2026, 8, 21, 20))!;
  assertEquals(between.inside, false);
  assertEquals(between.nextStart.getTime(), utc(2026, 8, 21, 22).getTime());
  // After midnight the rhythm continues: 03:00 next day.
  const late = windowStatus(spec, utc(2026, 8, 22, 1))!;
  assertEquals(late.nextStart.getTime(), utc(2026, 8, 22, 3).getTime());
});

Deno.test("interval before anchor waits for the first shift", () => {
  const spec: WorkScheduleSpec = {
    mode: "interval",
    every_hours: 5,
    length_min: 90,
    anchor: "2026-08-21T17:00:00Z",
  };
  const st = windowStatus(spec, utc(2026, 8, 21, 9))!;
  assertEquals(st.inside, false);
  assertEquals(st.nextStart.getTime(), utc(2026, 8, 21, 17).getTime());
  assertEquals(st.nextEnd.getTime(), utc(2026, 8, 21, 18, 30).getTime());
});

Deno.test("interval length >= period is continuous duty", () => {
  const spec: WorkScheduleSpec = {
    mode: "interval",
    every_hours: 4,
    length_min: 4 * 60,
    anchor: "2026-08-21T00:00:00Z",
  };
  const st = windowStatus(spec, utc(2026, 8, 22, 13, 17))!;
  assertEquals(st.inside, true);
  // The covering end keeps advancing with each period.
  assertEquals(st.nextEnd.getTime(), utc(2026, 8, 22, 16).getTime());
});

Deno.test("interval anchor edges are half-open both ways", () => {
  // Mirrors eval.rs interval_anchor_edges: every 2h for 60min from an
  // on-the-hour anchor — inside at the anchor, out exactly at the end.
  const spec: WorkScheduleSpec = {
    mode: "interval",
    every_hours: 2,
    length_min: 60,
    anchor: "2026-08-21T00:00:00Z",
  };
  const start = windowStatus(spec, utc(2026, 8, 21, 0))!;
  assertEquals(start.inside, true);
  assertEquals(start.nextEnd.getTime(), utc(2026, 8, 21, 1).getTime());
  const end = windowStatus(spec, utc(2026, 8, 21, 1))!;
  assertEquals(end.inside, false);
  assertEquals(end.nextStart.getTime(), utc(2026, 8, 21, 2).getTime());
});

Deno.test("windowStatus returns null for a broken anchor", () => {
  const spec = {
    mode: "interval",
    every_hours: 5,
    length_min: 30,
    anchor: "not-a-date",
  } as unknown as WorkScheduleSpec;
  assertEquals(windowStatus(spec, new Date()), null);
});
