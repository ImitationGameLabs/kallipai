// Reactive wrapper over the pure stamp core (stamp.ts). The tagma's
// configured IANA timezone is module-level $state so every render site that
// reads it recomputes when the setting changes. Components import the bound
// `formatStamp` from here; test-importable pure modules use the core with an
// explicit timezone instead (deno test does not compile runes).
import { formatStampInZone } from "./stamp.ts";

export const timezoneSetting = $state<{ value: string | null }>({
  value: null,
});

/** Fill the setting from the tagma (best-effort: a failure leaves the
 * browser-local default in effect rather than breaking the page). */
export const timezoneLoad = $state<{ done: boolean; ok: boolean }>({
  done: false,
  ok: false,
});

// Single-flight: concurrent callers share one GET; the guard clears on
// completion so a re-established session re-fetches (rebind = fresh read).
let inflight: Promise<void> | null = null;
export async function loadTimezoneSetting(
  getTimezone: () => Promise<{ timezone: string | null }>,
): Promise<void> {
  if (inflight) return inflight;
  inflight = (async () => {
    try {
      timezoneSetting.value = (await getTimezone()).timezone;
      timezoneLoad.done = true;
      timezoneLoad.ok = true;
    } catch {
      timezoneSetting.value = null;
      timezoneLoad.done = true;
      timezoneLoad.ok = false;
    }
  })().finally(() => {
    inflight = null;
  });
  return inflight;
}

/** Push a new value to the tagma and reflect it locally. Returns the server's
 * answer; a 400 (invalid IANA name) throws so the settings UI can render it. */
export async function saveTimezoneSetting(
  putTimezone: (tz: string | null) => Promise<{ timezone: string | null }>,
  value: string | null,
): Promise<string | null> {
  const confirmed = (await putTimezone(value)).timezone;
  timezoneSetting.value = confirmed;
  return confirmed;
}

/** Render an absolute timestamp in the configured timezone (see stamp.ts for
 * the resolution order). `opts` flows straight into Intl.DateTimeFormat. */
export function formatStamp(
  value: string | number,
  opts?: Intl.DateTimeFormatOptions,
): string {
  return formatStampInZone(value, opts, timezoneSetting.value);
}

export { timezoneCandidates } from "./stamp.ts";
