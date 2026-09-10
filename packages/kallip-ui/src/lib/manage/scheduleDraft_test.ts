// Behavior tests for the schedule draft seam: framing, dirty tracking,
// the save gate, and clock-switch reachability. Frame-math edge cases
// themselves live in the workSchedule tests; these lock the draft-side
// semantics.

import { assertEquals } from "@std/assert";
import type { WorkSchedule, WorkScheduleSpec } from "@kallipai/kallip-client";
import {
  applyFrame,
  canSave,
  defaultDraft,
  draftFrom,
  isDirty,
  reframe,
  reaches,
  specEq,
  warnMinutesValid,
} from "./scheduleDraft.ts";

const sched = (spec: WorkScheduleSpec): WorkSchedule => ({
  spec,
  id: "sched-1",
  created_at: "2026-08-26T00:00:00Z",
  pre_warn_minutes: 10,
  final_warn_minutes: 5,
  wake_prompt: "wake up",
  final_warn_prompt: null,
  status: "active",
});

Deno.test("applyFrame shifts a weekly spec into the display frame", () => {
  const wire: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b0000001,
    windows: [{ start_minute: 0, end_minute: 480 }],
  };
  const r = applyFrame(wire, 60);
  assertEquals(r.fellBack, false);
  assertEquals(r.spec, {
    mode: "weekly",
    days: 0b0000001,
    windows: [{ start_minute: 60, end_minute: 540 }],
  });
});

Deno.test(
  "applyFrame falls back to UTC for an unframeable full-day window",
  () => {
    const wire: WorkScheduleSpec = {
      mode: "weekly",
      days: 0b0000011,
      windows: [{ start_minute: 0, end_minute: 1440 }],
    };
    const r = applyFrame(wire, 60);
    assertEquals(r.fellBack, true);
    assertEquals(r.spec, wire);
  },
);

Deno.test("applyFrame normalizes a full-week full-day spec to always", () => {
  const wire: WorkScheduleSpec = {
    mode: "weekly",
    days: 0b1111111,
    windows: [{ start_minute: 0, end_minute: 1440 }],
  };
  const r = applyFrame(wire, 420);
  assertEquals(r.fellBack, false);
  assertEquals(r.spec, { mode: "always" });
});

Deno.test("draftFrom maps fields and normalizes null prompts to empty", () => {
  const { draft, fellBack } = draftFrom(
    sched({
      mode: "interval",
      every_hours: 6,
      length_min: 30,
      anchor: "2026-08-26T00:00:00Z",
    }),
    420,
  );
  assertEquals(fellBack, false);
  assertEquals(draft.pre_warn_minutes, 10);
  assertEquals(draft.final_warn_prompt, "");
  assertEquals(draft.spec.mode, "interval");
});

Deno.test("default draft is always-active with default warnings", () => {
  const d = defaultDraft();
  assertEquals(d.spec, { mode: "always" });
  assertEquals(d.pre_warn_minutes, 10);
  assertEquals(d.status, "active");
});

Deno.test("isDirty: false without a draft or before load, true unsaved", () => {
  assertEquals(isDirty(null, sched({ mode: "always" }), true, 0), false);
  assertEquals(
    isDirty(defaultDraft(), sched({ mode: "always" }), false, 0),
    false,
  );
  assertEquals(isDirty(defaultDraft(), null, true, 0), true);
});

Deno.test("isDirty: clean round-trip reads back not dirty", () => {
  const snap = sched({
    mode: "weekly",
    days: 0b0000001,
    windows: [{ start_minute: 0, end_minute: 480 }],
  });
  const { draft } = draftFrom(snap, 60);
  assertEquals(isDirty(draft, snap, true, 60), false);
  draft.pre_warn_minutes = 15;
  assertEquals(isDirty(draft, snap, true, 60), true);
});

Deno.test("isDirty: an unframeable snapshot counts as dirty", () => {
  const snap = sched({
    mode: "weekly",
    days: 0b0000011,
    windows: [{ start_minute: 0, end_minute: 1440 }],
  });
  const { draft } = draftFrom(snap, 60);
  assertEquals(isDirty(draft, snap, true, 60), true);
});

Deno.test("warnMinutesValid enforces positive integers with pre >= fin", () => {
  const d = defaultDraft();
  assertEquals(warnMinutesValid(d), true);
  d.pre_warn_minutes = d.final_warn_minutes;
  assertEquals(warnMinutesValid(d), true);
  d.final_warn_minutes = 0;
  assertEquals(warnMinutesValid(d), false);
  d.final_warn_minutes = 5.5;
  assertEquals(warnMinutesValid(d), false);
  assertEquals(warnMinutesValid(null), false);
});

Deno.test("canSave gates on every input", () => {
  const yes = {
    dirty: true,
    specError: null,
    warnValid: true,
    isSaving: false,
    wire: { mode: "always" } as WorkScheduleSpec,
  };
  assertEquals(canSave(yes), true);
  assertEquals(canSave({ ...yes, dirty: false }), false);
  assertEquals(canSave({ ...yes, specError: "days_empty" }), false);
  assertEquals(canSave({ ...yes, warnValid: false }), false);
  assertEquals(canSave({ ...yes, isSaving: true }), false);
  assertEquals(canSave({ ...yes, wire: null }), false);
});

Deno.test(
  "reframe round-trips a weekly spec and refuses the null family",
  () => {
    const spec = {
      mode: "weekly",
      days: 0b0000001,
      windows: [{ start_minute: 60, end_minute: 540 }],
    } as const;
    const back = reframe(spec, 60, 0);
    assertEquals(back, {
      mode: "weekly",
      days: 0b0000001,
      windows: [{ start_minute: 0, end_minute: 480 }],
    });
    const fullDay = {
      mode: "weekly",
      days: 0b0000011,
      windows: [{ start_minute: 60, end_minute: 60 + 1380 }],
    } as const;
    assertEquals(reframe(fullDay, 60, 0), {
      mode: "weekly",
      days: 0b0000011,
      windows: [{ start_minute: 0, end_minute: 1380 }],
    });
    // A full-day window on selected days has no equivalent in a frame
    // whose midnight falls inside it: the shifted end wraps onto the start.
    const fullDayUtc = {
      mode: "weekly",
      days: 0b0000011,
      windows: [{ start_minute: 0, end_minute: 1440 }],
    } as const;
    assertEquals(reframe(fullDayUtc, 0, 60), null);
  },
);

Deno.test("reaches: null spec always reaches, unframeable does not", () => {
  assertEquals(reaches(null, 0, 420), true);
  const fullDayUtc = {
    mode: "weekly",
    days: 0b0000011,
    windows: [{ start_minute: 0, end_minute: 1440 }],
  } as const;
  assertEquals(reaches(fullDayUtc, 0, 60), false);
  assertEquals(reaches(fullDayUtc, 0, 0), true);
});

Deno.test("specEq compares specs by JSON shape", () => {
  const a: WorkScheduleSpec = { mode: "always" };
  assertEquals(specEq(a, { mode: "always" }), true);
  assertEquals(
    specEq(a, {
      mode: "weekly",
      days: 1,
      windows: [{ start_minute: 0, end_minute: 60 }],
    }),
    false,
  );
});
