// WorkSchedule helpers for the schedules page: spec validation, UTC ⇄ local
// minute conversion, and a client-side port of the tagma evaluator
// (crates/kallip-tagma/src/work_schedule/eval.rs) so the status line and
// next-start preview agree with the backend without a round trip.
//
// All spec times are UTC minutes-of-day. Local editing is a pure offset
// shift of the wall clock; across a DST transition the local wall time of
// a stored UTC minute drifts by the offset change (annotated in the UI,
// not stored anywhere).
//
// Drift watch (correctness review 2026-08-21): this port is
// display-only — duty follows the backend evaluator. The two
// implementations currently agree branch-for-branch; known divergences
// are UI-side only (start_minute 1440, every_hours <= 23 cap, anchor
// minute alignment). If eval.rs changes a boundary, scan length, or
// window merge, mirror it here — and if this port ever gains behavior
// authority, replace both suites with a shared JSON fixture.

import type { WorkScheduleSpec } from "@kallipai/kallip-client";

export const DAY_MINUTES = 24 * 60;
export const MAX_LENGTH_MINUTES = 7 * DAY_MINUTES;
export const MAX_EVERY_HOURS = 23; // product decision; the backend accepts any every_hours >= 1

type DayMaskSpec = Extract<WorkScheduleSpec, { mode: "weekly" | "monthly" }>;
type IntervalSpec = Extract<WorkScheduleSpec, { mode: "interval" }>;

// --- validation (mirrors spec.rs; returns an i18n key or null) ---

export type SpecError =
  | "days_empty"
  | "days_range"
  | "minute_range"
  | "zero_window"
  | "every_hours_range"
  | "length_min_range";

export function validateSpec(spec: WorkScheduleSpec): SpecError | null {
  switch (spec.mode) {
    case "weekly":
    case "monthly": {
      if (spec.days === 0) return "days_empty";
      const max = spec.mode === "weekly" ? 0b0111_1111 : 2 ** 31 - 1;
      if (spec.days > max) return "days_range";
      return validateWindow(spec.start_minute, spec.end_minute);
    }
    case "interval": {
      if (
        !Number.isInteger(spec.every_hours) ||
        spec.every_hours < 1 ||
        spec.every_hours > MAX_EVERY_HOURS
      ) {
        return "every_hours_range";
      }
      if (
        !Number.isInteger(spec.length_min) ||
        spec.length_min < 1 ||
        spec.length_min > MAX_LENGTH_MINUTES
      ) {
        return "length_min_range";
      }
      return null;
    }
  }
}

function validateWindow(start: number, end: number): SpecError | null {
  // Mirrors spec.rs validate_window: start is the day's first minute on
  // (0..=1439); end is 1..=1440, where 1440 means "to the end of the day".
  // An end of 0 is never legal — "00:00" closes nothing.
  if (!Number.isInteger(start) || start < 0 || start >= DAY_MINUTES)
    return "minute_range";
  if (!Number.isInteger(end) || end < 1 || end > DAY_MINUTES)
    return "minute_range";
  if (start === end) return "zero_window";
  return null;
}

// --- minutes ⇄ "HH:MM" ---

export function minuteToHHMM(minute: number): string {
  const m = ((minute % DAY_MINUTES) + DAY_MINUTES) % DAY_MINUTES;
  return `${String(Math.floor(m / 60)).padStart(2, "0")}:${String(m % 60).padStart(2, "0")}`;
}

export function hhmmToMinute(text: string): number | null {
  const match = /^(\d{1,2}):(\d{2})$/.exec(text.trim());
  if (!match) return null;
  const h = Number(match[1]);
  const m = Number(match[2]);
  if (h > 24 || m > 59 || (h === 24 && m > 0)) return null;
  return h * 60 + m;
}

// --- UTC ⇄ local minute-of-day ---

/** Shift a minute-of-day by a fixed offset (positive = east of UTC). */
export function shiftMinute(minute: number, offsetMinutes: number): number {
  return (((minute + offsetMinutes) % DAY_MINUTES) + DAY_MINUTES) % DAY_MINUTES;
}

/** The browser's offset east of UTC right now, in minutes. */
export function localOffsetMinutes(now = new Date()): number {
  return -now.getTimezoneOffset();
}

/** "UTC+08:00"-style label for an offset in minutes. */
export function offsetLabel(offsetMinutes: number): string {
  const sign = offsetMinutes < 0 ? "-" : "+";
  const abs = Math.abs(offsetMinutes);
  const h = String(Math.floor(abs / 60)).padStart(2, "0");
  const m = String(abs % 60).padStart(2, "0");
  return `UTC${sign}${h}:${m}`;
}

// --- masks ⇄ lists ---

/** Weekday bits → ISO numbers (1 = Mon … 7 = Sun), Monday-first order. */
export function weekdayList(days: number): number[] {
  const out: number[] = [];
  for (let i = 0; i < 7; i++) if (days & (1 << i)) out.push(i + 1);
  return out;
}

/** Month-day bits → day-of-month numbers (1 … 31). */
export function monthDayList(days: number): number[] {
  const out: number[] = [];
  for (let i = 0; i < 31; i++) if (days & (1 << i)) out.push(i + 1);
  return out;
}

// --- evaluator (port of eval.rs window_status; UTC instants) ---

export interface WindowStatus {
  inside: boolean;
  nextStart: Date;
  nextEnd: Date;
}

export function windowStatus(
  spec: WorkScheduleSpec,
  now: Date,
): WindowStatus | null {
  switch (spec.mode) {
    case "interval":
      return evalInterval(spec, now);
    case "weekly":
    case "monthly":
      return evalCalendar(spec, now);
  }
}

function evalInterval(spec: IntervalSpec, now: Date): WindowStatus | null {
  const anchor = new Date(spec.anchor);
  if (Number.isNaN(anchor.getTime())) return null;
  const periodMs = spec.every_hours * 3_600_000;
  const lengthMs = spec.length_min * 60_000;
  const t = now.getTime();
  const anchorMs = anchor.getTime();
  if (t < anchorMs) {
    // The rotation has not started: the anchor opens the first shift.
    return {
      inside: false,
      nextStart: anchor,
      nextEnd: new Date(anchorMs + lengthMs),
    };
  }
  // Floor division keeps the phase exact across day boundaries.
  const k = Math.floor((t - anchorMs) / periodMs);
  const windowStart = anchorMs + k * periodMs;
  const windowEnd = windowStart + lengthMs;
  if (windowStart <= t && t < windowEnd) {
    return {
      inside: true,
      nextStart: new Date(windowStart + periodMs),
      nextEnd: new Date(windowEnd),
    };
  }
  const next = windowStart + periodMs;
  return {
    inside: false,
    nextStart: new Date(next),
    nextEnd: new Date(next + lengthMs),
  };
}

function evalCalendar(spec: DayMaskSpec, now: Date): WindowStatus | null {
  // Half-open [start, end); an end at or below the start crosses midnight
  // and belongs to the start day. Candidate days are scanned from
  // yesterday (its overnight window may still cover now) through a 70-day
  // horizon — the worst legal monthly gap (day 31 only, across February).
  const overnight = spec.end_minute <= spec.start_minute;
  const dayLen = overnight
    ? DAY_MINUTES - spec.start_minute + spec.end_minute
    : spec.end_minute - spec.start_minute;
  const dayMs = 86_400_000;
  const today = Date.UTC(
    now.getUTCFullYear(),
    now.getUTCMonth(),
    now.getUTCDate(),
  );
  const t = now.getTime();
  let nextStart: number | null = null;
  let nextEnd: number | null = null;
  let coveringEnd: number | null = null;
  for (let offset = -1; offset <= 70; offset++) {
    const dayStart = today + offset * dayMs;
    if (!firesOn(spec, dayStart)) continue;
    const ws = dayStart + spec.start_minute * 60_000;
    const we = ws + dayLen * 60_000;
    if (ws <= t && t < we) {
      // Overlapping windows merge into one covering end.
      coveringEnd = Math.max(coveringEnd ?? we, we);
    } else if (ws > t) {
      if (nextStart === null || ws < nextStart) nextStart = ws;
      if (nextEnd === null || we < nextEnd) nextEnd = we;
    }
  }
  if (coveringEnd !== null && nextStart !== null) {
    return {
      inside: true,
      nextStart: new Date(nextStart),
      nextEnd: new Date(coveringEnd),
    };
  }
  if (nextStart !== null && nextEnd !== null) {
    return {
      inside: false,
      nextStart: new Date(nextStart),
      nextEnd: new Date(nextEnd),
    };
  }
  return null;
}

function firesOn(spec: DayMaskSpec, utcDayStart: number): boolean {
  const d = new Date(utcDayStart);
  if (spec.mode === "weekly") {
    // ISO weekday 1 (Mon) … 7 (Sun); JS getUTCDay is 0 (Sun) … 6 (Sat).
    const iso = d.getUTCDay() === 0 ? 7 : d.getUTCDay();
    return (spec.days & (1 << (iso - 1))) !== 0;
  }
  return (spec.days & (1 << (d.getUTCDate() - 1))) !== 0;
}

// --- display formatting (chosen clock: UTC or browser-local) ---

/** "HH:MM" of an instant in the chosen clock. */
export function formatClock(d: Date, utc: boolean): string {
  return minuteToHHMM(
    utc
      ? d.getUTCHours() * 60 + d.getUTCMinutes()
      : d.getHours() * 60 + d.getMinutes(),
  );
}

/** Short day label ("Aug 22" / "8月22日") of an instant in the chosen clock. */
export function formatDay(d: Date, utc: boolean): string {
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    ...(utc ? { timeZone: "UTC" } : {}),
  }).format(d);
}
