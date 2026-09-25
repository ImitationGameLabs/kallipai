// Pure draft lifecycle for the schedules page: the editable draft's shape,
// how a wire spec is framed into it, and the predicates that gate saving.
// workSchedule.ts owns the frame math and validation; the page owns the
// $state draft, the clock preference, and localStorage — this seam keeps
// the logic directly testable.

import type { WorkSchedule, WorkScheduleSpec } from "@kallipai/kallipai-client";
import { fromFrame, toFrame } from "./workSchedule.ts";

// Structurally the wire spec minus readonly, so the draft can be edited
// in place; assigning into PutWorkScheduleRequest stays sound because
// the shapes are the same.
export type MutableWindow = { start_minute: number; end_minute: number };
export type MutableSpec =
  | { mode: "weekly"; days: number; windows: readonly MutableWindow[] }
  | {
      mode: "monthly";
      days: number;
      windows: readonly MutableWindow[];
    }
  | {
      mode: "interval";
      every_hours: number;
      length_min: number;
      anchor: string;
    }
  | { mode: "always" };

export type Draft = {
  spec: MutableSpec;
  pre_warn_minutes: number;
  final_warn_minutes: number;
  wake_prompt: string;
  // "" means the built-in default (wire null).
  final_warn_prompt: string;
  status: "active" | "paused";
};

export function defaultDraft(): Draft {
  return {
    spec: { mode: "always" },
    pre_warn_minutes: 10,
    final_warn_minutes: 5,
    wake_prompt: "",
    final_warn_prompt: "",
    status: "active",
  };
}

// Frame a plain (non-proxy) wire spec for display at `off` minutes east of
// UTC. fellBack marks the no-equivalent case (a partial-week full-day
// weekly outside UTC): the UTC frame is used for this visit and the caller
// locks the clock switch — the stored preference is left alone.
export function applyFrame(
  plain: WorkScheduleSpec,
  off: number,
): { spec: MutableSpec; fellBack: boolean } {
  const framed = toFrame(plain, off);
  if (framed === null) {
    return { spec: plain, fellBack: true };
  }
  return { spec: framed, fellBack: false };
}

// Build a fresh editable draft from the server's schedule. The spec may
// be a $state proxy: non-weekly modes pass it through, so the draft can
// alias the store's object — sound because every edit replaces the
// whole spec instead of mutating it.
export function draftFrom(
  s: WorkSchedule,
  off: number,
): { draft: Draft; fellBack: boolean } {
  const framed = applyFrame(s.spec, off);
  return {
    draft: {
      spec: framed.spec,
      pre_warn_minutes: s.pre_warn_minutes,
      final_warn_minutes: s.final_warn_minutes,
      wake_prompt: s.wake_prompt,
      final_warn_prompt: s.final_warn_prompt ?? "",
      status: s.status,
    },
    fellBack: framed.fellBack,
  };
}

// Field-level diff base: specs compare by their JSON shape because the
// frame math already normalizes order and ranges.
export const specEq = (a: WorkScheduleSpec, b: WorkScheduleSpec): boolean =>
  JSON.stringify(a) === JSON.stringify(b);

// Dirty against the server snapshot: an unsaved first draft counts as
// dirty, and so does a snapshot with no exact equivalent in the current
// frame (the user must resolve it before saving reads back clean).
export function isDirty(
  draft: Draft | null,
  snap: WorkSchedule | null,
  hasLoaded: boolean,
  off: number,
): boolean {
  if (!draft || !hasLoaded) return false;
  if (!snap) return true;
  const framedSnap = toFrame(snap.spec, off);
  return (
    framedSnap === null ||
    !specEq(draft.spec, framedSnap) ||
    draft.pre_warn_minutes !== snap.pre_warn_minutes ||
    draft.final_warn_minutes !== snap.final_warn_minutes ||
    draft.wake_prompt !== snap.wake_prompt ||
    draft.final_warn_prompt !== (snap.final_warn_prompt ?? "") ||
    draft.status !== snap.status
  );
}

export function warnMinutesValid(draft: Draft | null): boolean {
  if (!draft) return false;
  const pre = draft.pre_warn_minutes;
  const fin = draft.final_warn_minutes;
  return (
    Number.isInteger(pre) &&
    Number.isInteger(fin) &&
    pre > 0 &&
    fin > 0 &&
    pre >= fin
  );
}

// The save gate, one place: dirty, no spec validation error, warning
// minutes valid, no save in flight, and a wire-exact UTC equivalent.
export function canSave(input: {
  dirty: boolean;
  specError: string | null;
  warnValid: boolean;
  isSaving: boolean;
  wire: WorkScheduleSpec | null;
}): boolean {
  return (
    input.dirty &&
    input.specError === null &&
    input.warnValid &&
    !input.isSaving &&
    input.wire !== null
  );
}

// Re-express a draft spec in another frame; null when it has no exact
// equivalent there (the clock switch is refused then — the guard renders
// why). fromFrame/toFrame round-trip keeps what the page showed honest.
export function reframe(
  spec: MutableSpec,
  fromOff: number,
  toOff: number,
): MutableSpec | null {
  const wire = fromFrame(spec, fromOff);
  if (wire === null) return null;
  return toFrame(wire, toOff);
}

// Clock-switch reachability for the guard UI. A null draft (not loaded)
// always reaches.
export function reaches(
  spec: MutableSpec | null,
  fromOff: number,
  targetOff: number,
): boolean {
  if (!spec) return true;
  return reframe(spec, fromOff, targetOff) !== null;
}
