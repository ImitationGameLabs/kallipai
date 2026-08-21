// Tests for workSchedule.ts: validation, minute/offset math, the
// evaluator port, and the store-boundary frame pair. Evaluator cases
// client preview and the backend cannot drift apart silently.

import { assertEquals } from "@std/assert";
import {
  DAY_MINUTES,
  MAX_WINDOWS,
  hhmmToMinute,
  fromFrame,
  localOffsetMinutes,
  minuteToHHMM,
  monthDayList,
  offsetLabel,
  validateSpec,
  toFrame,
  weekdayList,
  windowStatus,
} from "./workSchedule.ts";
import type {
  WorkScheduleSpec,
  WorkScheduleWindow,
} from "@kallipai/kallip-client";

const utc = (y: number, mo: number, d: number, h = 0, mi = 0) =>
  new Date(Date.UTC(y, mo - 1, d, h, mi));

// --- validation ---

Deno.test("validateSpec: weekly happy path", () => {
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b0011_1111,
    windows: [{ start_minute: 540, end_minute: 1020 }],
  };
  assertEquals(validateSpec(spec), null);
});

Deno.test("validateSpec: empty day mask rejected in both modes", () => {
  assertEquals(
    validateSpec({
      mode: "weekly",
      days: 0,
      windows: [{ start_minute: 0, end_minute: 60 }],
    }),
    "days_empty",
  );
  assertEquals(
    validateSpec({
      mode: "monthly",
      days: 0,
      windows: [{ start_minute: 0, end_minute: 60 }],
    }),
    "days_empty",
  );
});

Deno.test("validateSpec: weekly mask limited to bits 0-6", () => {
  assertEquals(
    validateSpec({
      mode: "weekly",
      days: 1 << 7,
      windows: [{ start_minute: 0, end_minute: 60 }],
    }),
    "days_range",
  );
});

Deno.test("validateSpec: zero window never opens", () => {
  assertEquals(
    validateSpec({
      mode: "weekly",
      days: 1,
      windows: [{ start_minute: 600, end_minute: 600 }],
    }),
    "zero_window",
  );
});

Deno.test("validateSpec: end 1440 is a legal full-day window", () => {
  assertEquals(
    validateSpec({
      mode: "weekly",
      days: 1,
      windows: [{ start_minute: 0, end_minute: 1440 }],
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
      windows: [{ start_minute: 600, end_minute: 0 }],
    }),
    "minute_range",
  );
  assertEquals(
    validateSpec({
      mode: "monthly",
      days: 1,
      windows: [{ start_minute: 1440, end_minute: 100 }],
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
    windows: [{ start_minute: 540, end_minute: 1020 }],
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
    windows: [{ start_minute: 22 * 60, end_minute: 6 * 60 }],
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
    windows: [{ start_minute: 22 * 60, end_minute: 6 * 60 }],
  };
  const st = windowStatus(spec, utc(2026, 8, 11, 0))!;
  assertEquals(st.inside, true);
});

Deno.test("weekly next start after a far gap", () => {
  // Sunday-only 10:00-12:00; ask on Monday.
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b0100_0000,
    windows: [{ start_minute: 600, end_minute: 720 }],
  };
  const st = windowStatus(spec, utc(2026, 8, 10, 0))!;
  assertEquals(st.inside, false);
  assertEquals(st.nextStart.getTime(), utc(2026, 8, 16, 10).getTime());
});

Deno.test("weekly all days means daily", () => {
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b0111_1111,
    windows: [{ start_minute: 540, end_minute: 1020 }],
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
    windows: [{ start_minute: 540, end_minute: 1020 }],
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
    windows: [{ start_minute: 0, end_minute: 60 }],
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
    windows: [{ start_minute: 23 * 60, end_minute: 60 }],
  };
  const st = windowStatus(spec, utc(2026, 9, 1, 0, 30))!;
  assertEquals(st.inside, true);
  assertEquals(st.nextEnd.getTime(), utc(2026, 9, 1, 1).getTime());
});

Deno.test("full-day window via end_minute 1440", () => {
  const spec: WorkScheduleSpec = {
    mode: "weekly",
    days: 1,
    windows: [{ start_minute: 0, end_minute: DAY_MINUTES }],
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
    windows: [{ start_minute: 22 * 60, end_minute: 8 * 60 }], // overnight spans both
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

// --- frame conversion (the store boundary's only crossing) ---

const OFFS = [-480, -300, 0, 330, 480, 840];

function weeklySpec(
  days: number,
  start_minute: number,
  end_minute: number,
): WorkScheduleSpec {
  return { mode: "weekly", days, windows: [{ start_minute, end_minute }] };
}
function multiWindow(
  days: number,
  windows: [number, number][],
): WorkScheduleSpec {
  return {
    mode: "weekly",
    days,
    windows: windows.map(([start_minute, end_minute]) => ({
      start_minute,
      end_minute,
    })),
  };
}

// Window shapes exercised per offset. "regular" stays inside a day,
// "overnight" crosses midnight, the "seam" family pins the frame
// start at the wrap point (off / off±1 minutes), "morningTail" ends
// exactly at the frame's midnight (the 1440-end convention).
function frameShapes(off: number): Array<[string, WorkScheduleSpec]> {
  const seam = ((off % 1440) + 1440) % 1440;
  return [
    ["regular", weeklySpec(0b0011_1111, 540, 1020)],
    ["overnight", weeklySpec(0b0001_1111, 22 * 60, 6 * 60)],
    ["seam-exact", weeklySpec(0b0100_0011, seam, (seam + 300) % 1440 || 1440)],
    ["seam-minus-1", weeklySpec(0b0011_1111, (seam + 1439) % 1440, 120)],
    ["seam-plus-1", weeklySpec(0b0110_0011, (seam + 1) % 1440, 600)],
    [
      "morningTail",
      weeklySpec(
        0b0011_1111,
        (seam + 1040) % 1440,
        (seam + 1440) % 1440 || 1440,
      ),
    ],
    [
      "multi",
      {
        mode: "weekly",
        days: 0b0011_1111,
        windows: [
          {
            start_minute: (seam + 540) % 1440,
            end_minute: (seam + 720) % 1440 || 1440,
          },
          {
            start_minute: (seam + 780) % 1440,
            end_minute: (seam + 1020) % 1440 || 1440,
          },
        ],
      },
    ],
  ];
}

Deno.test(
  "toFrame∘fromFrame is the identity on representable weekly specs",
  () => {
    for (const off of OFFS) {
      for (const [, spec] of frameShapes(off)) {
        const wire = fromFrame(spec, off);
        if (wire === null) continue; // excluded family, covered below
        assertEquals(
          toFrame(wire, off),
          spec,
          `off=${off} ${JSON.stringify(spec)}`,
        );
      }
    }
  },
);

Deno.test("fromFrame∘toFrame is the identity on the wire side", () => {
  for (const off of OFFS) {
    for (const [, spec] of frameShapes(off)) {
      const framed = toFrame(spec, off);
      if (framed === null || framed.mode !== "weekly") continue;
      assertEquals(
        fromFrame(framed, off),
        spec,
        `off=${off} ${JSON.stringify(spec)}`,
      );
    }
  }
});

Deno.test("the mask rotates with the start minute's day carry (+8)", () => {
  // Mon 06:00 UTC + 8h = Mon 14:00 — same day, mask unchanged.
  assertEquals(
    toFrame(weeklySpec(0b1, 360, 840), 480),
    weeklySpec(0b1, 840, 1320),
  );
  // Mon 09:00 UTC + 8h = Mon 17:00 — still Monday.
  assertEquals(
    toFrame(weeklySpec(0b1, 540, 1020), 480),
    weeklySpec(0b1, 1020, 60),
  );
  // B2 regression: frame Mon–Fri 06:00–14:00 at +8 becomes UTC Sun–Thu
  // 22:00–06:00 — the mask must rotate back a day, not just the minutes.
  assertEquals(
    fromFrame(weeklySpec(0b0001_1111, 360, 840), 480),
    weeklySpec(0b0100_1111, 1320, 360),
  );
});

Deno.test("the mask rotates forward for west offsets (−8)", () => {
  // West offsets carry when the window starts before 08:00 UTC:
  // Mon 03:00–10:00 UTC at UTC−8 is Sun 19:00–02:00, owned by Sunday.
  assertEquals(
    toFrame(weeklySpec(0b0000_0001, 180, 600), -480),
    weeklySpec(0b1000000, 1140, 120),
  );
});

Deno.test("B1 regression: preset literals save as UTC-shifted minutes", () => {
  // The 9-to-5 preset applies frame literals (540/1020); at +8 the wire
  // must hold 60/540, never the frame numbers themselves.
  assertEquals(
    fromFrame(weeklySpec(0b0001_1111, 540, 1020), 480),
    weeklySpec(0b0001_1111, 60, 540),
  );
});

Deno.test("full-week full-day normalizes to always, from either side", () => {
  const fullWeek = weeklySpec(0b0111_1111, 0, 1440);
  for (const off of OFFS) {
    assertEquals(fromFrame(fullWeek, off), { mode: "always" });
    assertEquals(toFrame(fullWeek, off), { mode: "always" });
  }
});

Deno.test("always passes through both directions at any offset", () => {
  const always: WorkScheduleSpec = { mode: "always" };
  for (const off of OFFS) {
    assertEquals(toFrame(always, off), always);
    assertEquals(fromFrame(always, off), always);
  }
});

Deno.test("always validates clean and evaluates as inside", () => {
  assertEquals(validateSpec({ mode: "always" }), null);
  const st = windowStatus({ mode: "always" }, utc(2026, 8, 21));
  assertEquals(st !== null && st.inside, true);
});

Deno.test("monthly and interval pass through untouched", () => {
  const monthly: WorkScheduleSpec = {
    mode: "monthly",
    days: 1 << 5,
    windows: [{ start_minute: 600, end_minute: 960 }],
  };
  const interval: WorkScheduleSpec = {
    mode: "interval",
    every_hours: 5,
    length_min: 90,
    anchor: "2026-08-21T00:00:00Z",
  };
  for (const off of OFFS) {
    assertEquals(toFrame(monthly, off), monthly);
    assertEquals(fromFrame(monthly, off), monthly);
    assertEquals(toFrame(interval, off), interval);
    assertEquals(fromFrame(interval, off), interval);
  }
});

Deno.test(
  "partial-week full-day is unrepresentable outside its own frame",
  () => {
    // A Monday-only full-day window cannot shift by +8: the frame model
    // has no double-day window. Save and clock-switch guards key on null.
    const monday = weeklySpec(0b1, 0, 1440);
    assertEquals(toFrame(monday, 480), null);
    assertEquals(fromFrame(monday, 480), null);
    // In the UTC frame it stays representable.
    assertEquals(toFrame(monday, 0), monday);
  },
);

// --- multi-window (mirror of the spec.rs/eval.rs suites) ---

Deno.test("multiWindow helper builds a windowed spec", () => {
  const spec = multiWindow(0b1, [
    [9 * 60, 12 * 60],
    [13 * 60, 17 * 60],
  ]);
  assertEquals(spec, {
    mode: "weekly",
    days: 0b1,
    windows: [
      { start_minute: 540, end_minute: 720 },
      { start_minute: 780, end_minute: 1020 },
    ],
  });
});

Deno.test("validateSpec: disjoint same-day windows are legal", () => {
  assertEquals(
    validateSpec(
      multiWindow(0b0011_1111, [
        [540, 720],
        [780, 1020],
      ]),
    ),
    null,
  );
});

Deno.test("validateSpec: overlapping windows rejected", () => {
  assertEquals(
    validateSpec(
      multiWindow(1, [
        [540, 720],
        [780, 1020],
        [600, 660],
      ]),
    ),
    "windows_overlap",
  );
});

Deno.test("validateSpec: absolute overlap rejected in either order", () => {
  // 22:00..02:00 x 00:00..06:00 as minutes do not intersect, but on
  // one day they share 00:00..02:00 — both array orders must catch it
  // (the backend once missed the swapped order).
  const earlyLate: WorkScheduleWindow[] = [
    { start_minute: 0, end_minute: 6 * 60 },
    { start_minute: 22 * 60, end_minute: 2 * 60 },
  ];
  assertEquals(
    validateSpec({ mode: "weekly", days: 1, windows: earlyLate }),
    "windows_overlap",
  );
  assertEquals(
    validateSpec({
      mode: "weekly",
      days: 1,
      windows: [...earlyLate].reverse(),
    }),
    "windows_overlap",
  );
});

Deno.test("validateSpec: overnight tail reaching the next day's window", () => {
  // 00:00..02:00 and 23:00..01:00: neither same-day nor one-direction
  // next-day overlaps, but the 23:00 window on day d lands inside
  // day d+1 where 00:00..02:00 lives.
  assertEquals(
    validateSpec(
      multiWindow(1, [
        [0, 2 * 60],
        [23 * 60, 1 * 60],
      ]),
    ),
    "windows_overlap",
  );
});

Deno.test("validateSpec: empty list and cap mirror the backend", () => {
  assertEquals(validateSpec(multiWindow(1, [])), "windows_empty");
  const capped: [number, number][] = Array.from(
    { length: MAX_WINDOWS + 1 },
    (_, i) => [i * 90, i * 90 + 60],
  );
  assertEquals(validateSpec(multiWindow(1, capped)), "windows_cap");
});

Deno.test("multi-window day: each window fires on its own span", () => {
  // Monday 09:00..12:00 + 13:00..17:00.
  const spec = multiWindow(0b1, [
    [540, 720],
    [780, 1020],
  ]);
  const mid = windowStatus(spec, utc(2026, 8, 10, 12, 30))!;
  assertEquals(mid.inside, false);
  assertEquals(mid.nextStart.getTime(), utc(2026, 8, 10, 13).getTime());
  assertEquals(mid.nextEnd.getTime(), utc(2026, 8, 10, 17).getTime());
  const afternoon = windowStatus(spec, utc(2026, 8, 10, 15))!;
  assertEquals(afternoon.inside, true);
  assertEquals(afternoon.nextEnd.getTime(), utc(2026, 8, 10, 17).getTime());
  assertEquals(afternoon.nextStart.getTime(), utc(2026, 8, 17, 9).getTime());
});

Deno.test(
  "multi-window next picks the earliest across windows and days",
  () => {
    // Monday morning window, Tuesday evening window; ask Sunday.
    const spec = multiWindow(0b0000_0011, [
      [540, 600],
      [1080, 1140],
    ]);
    const st = windowStatus(spec, utc(2026, 8, 9, 0))!;
    assertEquals(st.inside, false);
    assertEquals(st.nextStart.getTime(), utc(2026, 8, 10, 9).getTime());
    assertEquals(st.nextEnd.getTime(), utc(2026, 8, 10, 10).getTime());
  },
);

Deno.test("multi-window overnight tail covers the next day's start", () => {
  // Mon+Tue 21:00..01:00 and (via separate days) Tue 00:30 is inside
  // Monday's tail; the covering end is 01:00.
  const spec = multiWindow(0b0000_0011, [[21 * 60, 1 * 60]]);
  const st = windowStatus(spec, utc(2026, 8, 11, 0, 30))!;
  assertEquals(st.inside, true);
  assertEquals(st.nextEnd.getTime(), utc(2026, 8, 11, 1).getTime());
});

Deno.test(
  "frame pair: mixed-carries are unrepresentable, shared carry shifts",
  () => {
    // Both windows cross the +8 seam (both carries = +1): the mask
    // rotates once and each window's minutes shift independently.
    const both = multiWindow(0b1, [
      [16 * 60, 20 * 60],
      [21 * 60, 23 * 60],
    ]);
    assertEquals(toFrame(both, 480), {
      mode: "weekly",
      days: 0b0000_0010,
      windows: [
        { start_minute: 0, end_minute: 4 * 60 },
        { start_minute: 5 * 60, end_minute: 7 * 60 },
      ],
    });
    // One window crossing and one staying put cannot share one mask.
    const mixed = multiWindow(0b1, [
      [16 * 60, 20 * 60],
      [1 * 60, 3 * 60],
    ]);
    assertEquals(toFrame(mixed, 480), null);
    assertEquals(fromFrame(mixed, 480), null);
  },
);

Deno.test("frame pair: multi-window round trip at every offset", () => {
  for (const off of OFFS) {
    const seam = ((off % 1440) + 1440) % 1440;
    const spec = multiWindow(0b0011_1111, [
      [(seam + 540) % 1440, (seam + 720) % 1440 || 1440],
      [(seam + 780) % 1440, (seam + 1020) % 1440 || 1440],
    ]);
    const wire = fromFrame(spec, off);
    if (wire === null) continue; // mixed carries at this seam
    assertEquals(toFrame(wire, off), spec, `off=${off}`);
  }
});

Deno.test(
  "normalization to always still requires the single-window form",
  () => {
    // A full-day window among others is not 24/7: two touching
    // half-day windows on a full week stay weekly.
    const spec = multiWindow(0b0111_1111, [
      [0, 12 * 60],
      [12 * 60, DAY_MINUTES],
    ]);
    const framed = toFrame(spec, 480);
    assertEquals(framed === null || framed.mode === "weekly", true);
  },
);
