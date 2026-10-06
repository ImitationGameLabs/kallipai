// Pure core for central absolute-timestamp rendering (test-importable: no
// runes). Resolution order: the explicit timezone (the tagma's configured
// IANA name, passed by the reactive wrapper), then the browser's local zone
// (the natural reading frame), then a UTC fallback for the pathological case
// where even the default formatter is unavailable.
import { getLocale } from "../../paraglide/runtime.js";

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
      return new Intl.DateTimeFormat(locale, {
        timeZone: timezone,
        ...rest,
      }).format(date);
    } catch {
      // Unknown stored zone: fall through to the browser default.
    }
  }
  try {
    return new Intl.DateTimeFormat(locale, rest).format(date);
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
