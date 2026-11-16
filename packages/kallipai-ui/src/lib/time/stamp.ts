// Pure core for central absolute-timestamp rendering (test-importable: no
// runes). Resolution order: the explicit timezone (the tagma's configured
// IANA name, passed by the reactive wrapper), then the browser's local zone
// (the natural reading frame), then a UTC fallback for the pathological case
// where even the default formatter is unavailable.
import { getLocale } from "../../paraglide/runtime.js";

/** The time-of-day shape every transcript row label uses. A spoken row
 * sentence must match the label beside it, so both read this one shape. */
export const ROW_TIME: Intl.DateTimeFormatOptions = {
  hour: "numeric",
  minute: "2-digit",
};

/** The full stamp shape: date plus time to the second. Ledger views that
 * put several events of one day in a column read this, so a row's position
 * in the sequence is legible instead of repeating the same date. */
export const STAMP_SECONDS: Intl.DateTimeFormatOptions = {
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
};

// One formatter per (locale, zone, shape). A transcript renders the same shape
// for every row and /chats formats a whole ledger whenever it changes, so
// building a formatter per call costs tens of milliseconds there. The locale is
// part of the key because a language switch has to change the words.
const FORMATTERS = new Map<string, Intl.DateTimeFormat>();

function formatterFor(
  locale: string,
  shape: Intl.DateTimeFormatOptions,
  timeZone?: string,
): Intl.DateTimeFormat {
  const key = `${locale}|${timeZone ?? ""}|${JSON.stringify(shape)}`;
  let formatter = FORMATTERS.get(key);
  if (formatter === undefined) {
    formatter = new Intl.DateTimeFormat(
      locale,
      timeZone === undefined ? shape : { timeZone, ...shape },
    );
    FORMATTERS.set(key, formatter);
  }
  return formatter;
}

export function formatStampInZone(
  value: string | number,
  opts: Intl.DateTimeFormatOptions | undefined,
  timezone: string | null | undefined,
): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return String(value);
  const locale = getLocale();
  // Strip any caller-supplied timeZone: the explicit argument owns the
  // resolution order; an override would silently bypass the fallback chain.
  const { timeZone: _ignored, ...rest } = opts ?? {};
  if (timezone) {
    try {
      return formatterFor(locale, rest, timezone).format(date);
    } catch {
      // Unknown stored zone: fall through to the browser default.
    }
  }
  try {
    return formatterFor(locale, rest).format(date);
  } catch {
    return date.toISOString();
  }
}

/** Candidate zones for the settings input. Browsers without the API get an
 * empty datalist (free text still works). */
export function timezoneCandidates(): string[] {
  const values = (
    Intl as unknown as { supportedValuesOf?: (k: string) => string[] }
  ).supportedValuesOf;
  return values ? values("timeZone") : [];
}

/** The local end-of-day instant of a `YYYY-MM-DD` day, or null when the
 * string is not a plain date (the expiry semantics: a grant for a day
 * stays valid through that whole day). Parsed by parts -- `new Date`
 * would read the bare date as UTC midnight and shift the wall day. */
export function endOfDayIso(day: string): string | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(day);
  if (m === null) return null;
  return new Date(
    Number(m[1]),
    Number(m[2]) - 1,
    Number(m[3]),
    23,
    59,
    59,
    999,
  ).toISOString();
}

/** The local today as a `YYYY-MM-DD` string (the date input's min). */
export function todayIso(): string {
  const now = new Date();
  const month = `${now.getMonth() + 1}`.padStart(2, "0");
  const day = `${now.getDate()}`.padStart(2, "0");
  return `${now.getFullYear()}-${month}-${day}`;
}
