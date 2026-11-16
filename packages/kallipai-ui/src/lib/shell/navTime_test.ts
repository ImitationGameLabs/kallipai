// The recents stamp is pure: a fixed `now` covers every branch. The app
// locale is pinned through paraglide's test seam, so these assertions fail if
// the stamp ever falls back to the browser's language or to one fixed hour
// cycle instead of the reader's.

import {
  assertEquals,
  assertMatch,
  assertNotEquals,
  assertStringIncludes,
} from "@std/assert";
import { getLocale, overwriteGetLocale } from "../../paraglide/runtime.js";
import { formatRecentTime } from "./navTime.ts";

/** Run `body` with `getLocale()` pinned, then put the previous reader back:
 *  the seam is process-wide and other test files share the process. */
function withLocale(locale: "en" | "zh", body: () => void): void {
  const previous = getLocale;
  overwriteGetLocale(() => locale);
  try {
    body();
  } finally {
    overwriteGetLocale(previous);
  }
}

const NOW = new Date(2026, 10, 17, 18, 0).getTime();
const TODAY = new Date(2026, 10, 17, 14, 32).getTime();
const THREE_DAYS_AGO = new Date(2026, 10, 14, 9, 0).getTime();
const A_WEEK_AGO = new Date(2026, 10, 10, 12, 0).getTime();
const TOMORROW = new Date(2026, 10, 18, 8, 15).getTime();

Deno.test("today reads as a wall clock time", () => {
  withLocale("zh", () => {
    assertMatch(formatRecentTime(TODAY, NOW), /^\d{1,2}:\d{2}$/);
  });
});

Deno.test("the weekday word follows the app locale", () => {
  let zh = "";
  let en = "";
  withLocale("zh", () => (zh = formatRecentTime(THREE_DAYS_AGO, NOW)));
  withLocale("en", () => (en = formatRecentTime(THREE_DAYS_AGO, NOW)));
  assertMatch(zh, /^(周|星期)/u);
  assertMatch(en, /^[A-Z][a-z]{2}$/);
  assertNotEquals(zh, en);
});

Deno.test("the clock keeps the reader's hour cycle", () => {
  let zh = "";
  let en = "";
  withLocale("zh", () => (zh = formatRecentTime(TODAY, NOW)));
  withLocale("en", () => (en = formatRecentTime(TODAY, NOW)));
  assertEquals(/[AP]M/i.test(zh), false);
  assertMatch(en, /[AP]M/);
});

Deno.test("seven days back already reads as year and month", () => {
  withLocale("zh", () => {
    const stamp = formatRecentTime(A_WEEK_AGO, NOW);
    assertStringIncludes(stamp, "2026");
    assertStringIncludes(stamp, "11");
  });
});

Deno.test("a stamp from a future clock reads as today's time", () => {
  withLocale("zh", () => {
    assertMatch(formatRecentTime(TOMORROW, NOW), /^\d{1,2}:\d{2}$/);
  });
});

Deno.test("an old stamp spells the month, in the locale's own order", () => {
  const at = new Date(2025, 0, 20, 12, 0).getTime();
  withLocale("en", () => {
    const stamp = formatRecentTime(at, NOW);
    assertStringIncludes(stamp, "2025");
    // A short month name, not a two-digit number: `01/2025` would read as a
    // day and a year to an English reader.
    assertStringIncludes(stamp, "Jan");
  });
  withLocale("zh", () => {
    const stamp = formatRecentTime(at, NOW);
    assertStringIncludes(stamp, "2025");
    assertStringIncludes(stamp, "1月");
  });
});

Deno.test("the clock reads the passed-in zone, not the browser's", () => {
  // 04:00Z is 04:00 in UTC and 12:00 in Shanghai; the browser's own zone
  // plays no part in either answer.
  const at = Date.UTC(2026, 10, 17, 4, 0);
  const now = at + 3_600_000;
  let utc = "";
  let shanghai = "";
  withLocale("en", () => {
    utc = formatRecentTime(at, now, "UTC");
    shanghai = formatRecentTime(at, now, "Asia/Shanghai");
  });
  assertMatch(utc, /^\d{1,2}:\d{2}/);
  assertMatch(shanghai, /^\d{1,2}:\d{2}/);
  assertNotEquals(utc, shanghai);
});

Deno.test("the day bucket follows the zone across a midnight", () => {
  // 15:00Z is still the 17th in UTC and already 23:00 on the 17th in
  // Shanghai; an hour later Shanghai is on the 18th while UTC is on the 17th.
  // The same instant therefore reads as today's clock in UTC and as a past
  // day in Shanghai.
  const at = Date.UTC(2026, 10, 17, 15, 0);
  const now = Date.UTC(2026, 10, 17, 16, 0);
  let utc = "";
  let shanghai = "";
  withLocale("en", () => {
    utc = formatRecentTime(at, now, "UTC");
    shanghai = formatRecentTime(at, now, "Asia/Shanghai");
  });
  assertMatch(utc, /^\d{1,2}:\d{2}/);
  assertMatch(shanghai, /^[A-Z][a-z]{2}$/);
  assertNotEquals(utc, shanghai);
});
