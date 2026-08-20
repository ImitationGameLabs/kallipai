import {
  manage_schedules_days_daily,
  manage_schedules_days_list,
  manage_schedules_days_sep,
  manage_schedules_days_weekdays,
  manage_schedules_days_weekend,
  manage_schedules_dow_0,
  manage_schedules_dow_1,
  manage_schedules_dow_2,
  manage_schedules_dow_3,
  manage_schedules_dow_4,
  manage_schedules_dow_5,
  manage_schedules_dow_6,
  manage_schedules_next_day,
  manage_schedules_summary_days_time,
  manage_schedules_summary_split,
} from "../../paraglide/messages.js";
// Plain-form translation for the schedule crons: the structured editor
// (ScheduleForm) edits a deliberately small subset of the backend's cron
// grammar, and these helpers convert between that subset and the stored
// `M H * * D` expressions. Anything outside the subset (steps, dom/month
// parts, multi-value hours) stays in the raw-cron "advanced" lane and is
// rendered verbatim rather than mistranslated.
//
// Subset shape, per field: minute = one value 0-59, hour = one value
// 0-23, dom = "*", month = "*", dow = any comma list of single values
// 0-6 (0 = Sunday), sorted ascending at compile time. Ranges in dow
// ("1-5") parse into their value set so the summary can still speak
// about the same days, but only lists are produced.

export type SubsetForm = {
  /** Minute of the hour, 0-59. */
  minute: number;
  /** Hour of the day, 0-23. */
  hour: number;
  /** Day-of-week set as ascending unique values, 0-6, 0 = Sunday. */
  dows: number[];
};

const DOW_LABELS = [
  manage_schedules_dow_0,
  manage_schedules_dow_1,
  manage_schedules_dow_2,
  manage_schedules_dow_3,
  manage_schedules_dow_4,
  manage_schedules_dow_5,
  manage_schedules_dow_6,
] as const;

function parseMinuteOrHour(
  part: string,
  lo: number,
  hi: number,
): number | null {
  if (!/^\d{1,2}$/.test(part)) return null;
  const v = Number(part);
  if (v < lo || v > hi) return null;
  return v;
}

function parseDowPart(part: string): number[] | null {
  const out: number[] = [];
  // "*" is the canonical every-day form.
  if (part === "*") return [0, 1, 2, 3, 4, 5, 6];
  for (const piece of part.split(",")) {
    // Accept "5" and "1-3"; reject anything else (steps, names).
    const range = /^(\d)(?:-(\d))?$/.exec(piece);
    if (!range) return null;
    const lo = Number(range[1]);
    const hi = range[2] === undefined ? lo : Number(range[2]);
    if (lo < 0 || lo > 6 || hi < 0 || hi > 6 || hi < lo) return null;
    for (let d = lo; d <= hi; d++) out.push(d);
  }
  return out.length > 0 ? out : null;
}

/** Parse a stored cron string into the editor subset; null when outside. */
export function parseSubset(cron: string): SubsetForm | null {
  const parts = cron.trim().split(/\s+/);
  if (parts.length !== 5) return null;
  const [minute = "", hour = "", dom = "", month = "", dow = ""] = parts;
  if (dom !== "*" || month !== "*") return null;
  const m = parseMinuteOrHour(minute, 0, 59);
  const h = parseMinuteOrHour(hour, 0, 23);
  const dows = parseDowPart(dow);
  if (m === null || h === null || dows === null) return null;
  const unique = [...new Set(dows)].sort((a, b) => a - b);
  return { minute: m, hour: h, dows: unique };
}

/** Compile the editor subset back into a canonical `M H * * D` string. */
export function compileSubset(form: SubsetForm): string {
  const dows = [...new Set(form.dows)].sort((a, b) => a - b);
  const dowField = dows.length === 7 ? "*" : dows.join(",");
  return `${form.minute} ${form.hour} * * ${dowField}`;
}

function pad2(n: number): string {
  return String(n).padStart(2, "0");
}

function dowPhrase(dows: readonly number[]): string {
  // Recognised sets get their plain name; anything else falls back to a
  // sorted day list via the per-day keys.
  if (dows.length === 7) return manage_schedules_days_daily();
  if (dows.join(",") === "1,2,3,4,5") return manage_schedules_days_weekdays();
  if (dows.join(",") === "0,6") return manage_schedules_days_weekend();
  return manage_schedules_days_list({
    days: dows
      .map((d) => DOW_LABELS[d]?.() ?? "")
      .join(manage_schedules_days_sep()),
  });
}

/**
 * Human summary for a start/end pair. Returns null when either side is
 * outside the subset (caller renders the raw cron verbatim instead).
 */
export function describeCron(
  startCron: string,
  endCron: string,
): string | null {
  const start = parseSubset(startCron);
  const end = parseSubset(endCron);
  if (!start || !end) return null;
  const sameDayTime =
    end.hour > start.hour ||
    (end.hour === start.hour && end.minute > start.minute);
  // A zero-width window (end == start never fires: the engine compares
  // next_end < next_start strictly) has no honest summary — fall back to
  // the raw cron.
  if (end.hour === start.hour && end.minute === start.minute) return null;
  const startText = `${pad2(start.hour)}:${pad2(start.minute)}`;
  const endText = `${pad2(end.hour)}:${pad2(end.minute)}`;
  const timeRange = sameDayTime
    ? `${startText}–${endText}`
    : `${startText}–${manage_schedules_next_day({ time: endText })}`;
  if (sameDays(start.dows, end.dows)) {
    return manage_schedules_summary_days_time({
      days: dowPhrase(start.dows),
      range: timeRange,
    });
  }
  return manage_schedules_summary_split({
    startDays: dowPhrase(start.dows),
    start: startText,
    endDays: dowPhrase(end.dows),
    end: endText,
  });
}

function sameDays(a: readonly number[], b: readonly number[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}

/**
 * Next start fire strictly after `now` (UTC), or null outside the subset.
 * Pure date arithmetic — the subset has no dom/month parts, so this is
 * weekday scanning only; no timezone or DST conversion is involved.
 */
export function nextStart(startCron: string, now: Date): Date | null {
  const start = parseSubset(startCron);
  if (!start) return null;
  // Candidate = today at start time; advance to the next matching weekday.
  const c = new Date(
    Date.UTC(
      now.getUTCFullYear(),
      now.getUTCMonth(),
      now.getUTCDate(),
      start.hour,
      start.minute,
    ),
  );
  const dowSet = new Set(start.dows);
  for (let i = 0; i < 8; i++) {
    if (c.getTime() > now.getTime() && dowSet.has(c.getUTCDay())) return c;
    c.setUTCDate(c.getUTCDate() + 1);
  }
  return null;
}
