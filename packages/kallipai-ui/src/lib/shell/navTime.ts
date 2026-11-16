// The recents rows carry a short stamp of when each conversation last moved.
// Three shapes cover the range a sidebar column can show: the clock for today,
// the weekday name for the past week, and year/month beyond it. Intl supplies
// every word in the app locale, and the clock reads ROW_TIME, the shape
// transcript rows use.
//
// The day bucket and the clock both read the display zone: the configured IANA
// zone when there is one, the browser's otherwise. Bucketing on the browser's
// calendar while the clock renders elsewhere would put yesterday's hour on a
// row labelled today near midnight.

import { formatStampInZone, ROW_TIME } from "../time/stamp.ts";

const DAY_MS = 86_400_000;

const DAY_SHAPE: Intl.DateTimeFormatOptions = {
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
};
const WEEKDAY_SHAPE: Intl.DateTimeFormatOptions = { weekday: "short" };
// `short` rather than a two-digit month: `10/2026` reads as a day and a year
// in en, `Oct 2026` does not. zh keeps its own order (`2026年10月`) either way.
const YEAR_MONTH_SHAPE: Intl.DateTimeFormatOptions = {
  year: "numeric",
  month: "short",
};

/** The calendar day `at` falls on, counted in `zone`. The parts come from Intl
 *  so an unknown stored zone falls back the way every other stamp does, and
 *  the index is built in UTC: a local day is 23 or 25 hours long across a DST
 *  shift, so subtracting local midnights would drift by one. */
function dayIndex(at: number, zone: string | null | undefined): number {
  const { year, month, day } = dayParts(at, zone);
  return Math.round(Date.UTC(year, month - 1, day) / DAY_MS);
}

// The day-parts formatter reads numbers only, in a fixed locale, so the zone
// alone decides which one applies.
const DAY_FORMATTERS = new Map<string, Intl.DateTimeFormat>();

function dayFormatter(timeZone?: string): Intl.DateTimeFormat {
  const key = timeZone ?? "";
  let formatter = DAY_FORMATTERS.get(key);
  if (formatter === undefined) {
    formatter = new Intl.DateTimeFormat("en-CA", { ...DAY_SHAPE, timeZone });
    DAY_FORMATTERS.set(key, formatter);
  }
  return formatter;
}

function dayParts(
  at: number,
  zone: string | null | undefined,
): { year: number; month: number; day: number } {
  const date = new Date(at);
  const read = (timeZone?: string) => {
    const parts = dayFormatter(timeZone).formatToParts(date);
    const pick = (type: string) =>
      Number(parts.find((part) => part.type === type)?.value);
    return { year: pick("year"), month: pick("month"), day: pick("day") };
  };
  if (zone) {
    try {
      return read(zone);
    } catch {
      // Unknown stored zone: fall through to the browser default.
    }
  }
  try {
    return read();
  } catch {
    return {
      year: date.getUTCFullYear(),
      month: date.getUTCMonth() + 1,
      day: date.getUTCDate(),
    };
  }
}

/** A short stamp for a recents row: `9:32` today, `Tue` inside the last week,
 *  `Nov 2025` beyond it (the order of month and year is the locale's call, not
 *  ours). A stamp in the future (clock skew) reads as today's time. `zone` is
 *  the passed-in display zone (`navTime.svelte.ts` reads the configured one);
 *  null or undefined means the browser's. `now` is a seam for tests. */
export function formatRecentTime(
  at: number,
  now: number = Date.now(),
  zone: string | null | undefined = undefined,
): string {
  const days = dayIndex(now, zone) - dayIndex(at, zone);
  if (days <= 0) return formatStampInZone(at, ROW_TIME, zone);
  if (days < 7) return formatStampInZone(at, WEEKDAY_SHAPE, zone);
  return formatStampInZone(at, YEAR_MONTH_SHAPE, zone);
}
