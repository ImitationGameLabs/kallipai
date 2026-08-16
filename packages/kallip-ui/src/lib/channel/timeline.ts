// Timeline grouping for rendered chat lines: decides, per line, whether to show
// a date divider ("Today" / "Yesterday" / locale date) and/or a time label
// ("14:30") above it, so consecutive messages within a short window share one
// timestamp instead of repeating per line. Pure (no DOM, no Svelte); unit-tested
// in timeline_test.ts.
//
// Lines without a `createdAt` (system notices, old cached rows from before the
// field existed) are transparent: they render no marker and do NOT reset the
// group, so a stray system line between two messages doesn't split them.

import { getLocale } from "../../paraglide/runtime.js";
import {
  timeline_today,
  timeline_yesterday,
} from "../../paraglide/messages.js";
/** Per-line render hints. Both absent (an empty object) means "render nothing
 * above this line." */
export interface TimelineMarker {
  /** Shown when the calendar day (in the browser's local timezone) differs
   * from the previous dated line. */
  readonly dateDivider?: string;
  /** Shown at the start of a time group: the first dated line, or any line
   * whose send time is more than `groupWindowMs` after the previous dated
   * line (and whenever a new day begins). */
  readonly timeLabel?: string;
}

interface TimelineOpts {
  /** Max gap within which consecutive messages share one time label.
   * Default 5 minutes. */
  readonly groupWindowMs?: number;
  /** Epoch ms treated as "now" (for Today/Yesterday). Defaults to `Date.now()`;
   * tests pass a fixed value. */
  readonly now?: number;
}

const DEFAULT_GROUP_WINDOW_MS = 5 * 60_000;

// Locale formatters are process-constant; hoist them so a recompute on every
// transcript mutation (the `$derived` in ChannelChatPage) doesn't rebuild them.
const dayFmt = new Intl.DateTimeFormat(getLocale(), {
  month: "short",
  day: "numeric",
});
const timeFmt = new Intl.DateTimeFormat(getLocale(), {
  hour: "numeric",
  minute: "2-digit",
});

export function timelineMarkers(
  lines: readonly { createdAt?: string }[],
  opts?: TimelineOpts,
): TimelineMarker[] {
  const groupWindow = opts?.groupWindowMs ?? DEFAULT_GROUP_WINDOW_MS;
  const now = opts?.now ?? Date.now();

  // Day key in the browser's local timezone -- compared as a string so the
  // divider respects the user's zone, not UTC.
  const dayKey = (ms: number): string => {
    const d = new Date(ms);
    return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
  };
  const todayKey = dayKey(now);
  const yesterdayDate = new Date(now);
  yesterdayDate.setDate(yesterdayDate.getDate() - 1);
  const yesterdayKey = dayKey(yesterdayDate.getTime());

  const dayLabel = (ms: number): string => {
    const k = dayKey(ms);
    if (k === todayKey) return timeline_today();
    if (k === yesterdayKey) return timeline_yesterday();
    return dayFmt.format(ms);
  };

  const out: TimelineMarker[] = [];
  let prevDayKey: string | undefined;
  let prevMs: number | undefined;
  for (const line of lines) {
    const ms = line.createdAt === undefined ? NaN : Date.parse(line.createdAt);
    if (Number.isNaN(ms)) {
      // Undated line: no marker, does not reset the group.
      out.push({});
      continue;
    }
    const k = dayKey(ms);
    const dateDivider =
      prevDayKey === undefined || k !== prevDayKey ? dayLabel(ms) : undefined;
    const timeLabel =
      prevMs === undefined ||
      ms - prevMs > groupWindow ||
      dateDivider !== undefined
        ? timeFmt.format(ms)
        : undefined;
    out.push({
      ...(dateDivider !== undefined && { dateDivider }),
      ...(timeLabel !== undefined && { timeLabel }),
    });
    prevDayKey = k;
    prevMs = ms;
  }
  return out;
}
