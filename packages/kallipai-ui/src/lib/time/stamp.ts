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
