// Timeline grouping tests. Assertions stay locale-independent: `dateDivider`
// compares the fixed strings ("Today") or just presence, and `timeLabel` checks
// presence (its exact text depends on the runtime locale). "Today" also
// assumes paraglide's default en locale (no PARAGLIDE_LOCALE cookie/env in
// the test runner).

import { assertEquals } from "@std/assert";
import { timelineMarkers } from "./timeline.ts";

// Fixed "now" so Today/Yesterday is deterministic; all the same-day fixtures
// below fall on the same calendar day as `now` in every timezone.
const NOW = Date.parse("2026-07-26T12:00:00Z");
const T = (iso: string): { createdAt: string } => ({ createdAt: iso });
const UNDATED = {};

Deno.test("empty input yields no markers", () => {
  assertEquals(timelineMarkers([], { now: NOW }), []);
});

Deno.test("first dated line shows a Today divider and a time label", () => {
  const m = timelineMarkers([T("2026-07-26T12:00:00Z")], { now: NOW });
  assertEquals(m.length, 1);
  assertEquals(m[0]!.dateDivider, "Today");
  assertEquals(m[0]!.timeLabel !== undefined, true);
});

Deno.test("messages within the window share one time label (grouped)", () => {
  const m = timelineMarkers(
    [T("2026-07-26T12:00:00Z"), T("2026-07-26T12:03:00Z")],
    { now: NOW },
  );
  assertEquals(m[0]!.dateDivider, "Today");
  assertEquals(m[0]!.timeLabel !== undefined, true);
  // Second line is within 5 min -> no new divider, no new time label.
  assertEquals(m[1]!.dateDivider, undefined);
  assertEquals(m[1]!.timeLabel, undefined);
});

Deno.test("a gap larger than the window starts a new time group", () => {
  const m = timelineMarkers(
    [T("2026-07-26T12:00:00Z"), T("2026-07-26T12:07:00Z")],
    { now: NOW },
  );
  assertEquals(m[0]!.timeLabel !== undefined, true);
  assertEquals(m[1]!.dateDivider, undefined); // still the same day
  assertEquals(m[1]!.timeLabel !== undefined, true); // but a new time group
});

Deno.test("undated lines are transparent to grouping", () => {
  // A system/old line between two messages within the window must not split
  // them, and itself renders no marker.
  const m = timelineMarkers(
    [T("2026-07-26T12:00:00Z"), UNDATED, T("2026-07-26T12:03:00Z")],
    { now: NOW },
  );
  assertEquals(m[0]!.timeLabel !== undefined, true);
  assertEquals(m[1], {});
  assertEquals(m[2]!.dateDivider, undefined);
  assertEquals(m[2]!.timeLabel, undefined);
});

Deno.test("a different calendar day triggers a date divider", () => {
  // A week-old line is a different calendar day from `now` in every timezone.
  const m = timelineMarkers(
    [T("2026-07-26T12:00:00Z"), T("2026-07-19T12:00:00Z")],
    { now: NOW },
  );
  assertEquals(m[0]!.dateDivider, "Today");
  assertEquals(m[1]!.dateDivider !== undefined, true);
  assertEquals(m[1]!.dateDivider !== "Today", true);
  assertEquals(m[1]!.timeLabel !== undefined, true);
});

Deno.test("groupWindowMs is configurable", () => {
  // A 3-minute gap groups under the default 5-min window but splits under 1.
  const lines = [T("2026-07-26T12:00:00Z"), T("2026-07-26T12:03:00Z")];
  assertEquals(timelineMarkers(lines, { now: NOW })[1]!.timeLabel, undefined);
  assertEquals(
    timelineMarkers(lines, { now: NOW, groupWindowMs: 60_000 })[1]!
      .timeLabel !== undefined,
    true,
  );
});
