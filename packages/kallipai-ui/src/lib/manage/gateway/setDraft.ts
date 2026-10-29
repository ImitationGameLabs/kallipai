// The set dialogs' shared draft arithmetic. Each dialog owns the state;
// these helpers own the rules (what a reorder does at the edges, which
// members a save has to PUT, and in what shape), so the tests can drive
// them without a browser and neither page re-derives them inline.

import type { Modality } from "@kallipai/kallipai-client";
import { MODALITY_ORDER, normalizeModalities } from "../compute.ts";

/** The part of a member row every face shares: the fields the form edits
 * and the id that names the row. The catalog and a user's own collection
 * both arrive in this shape; a user row may carry no modality list at all,
 * which reads as text only. */
export interface DraftRow {
  profile_id: string;
  /** The provider family both faces carry (the card shows it). */
  family: string;
  model: string;
  max_context_window: number | null;
  effort: string | null;
  modalities: readonly string[] | null;
  /** The catalog's rows name an owner and carry the served flag; a
   * user's own collection has neither, so both are optional. */
  owner?: string;
  parked?: boolean;
}

/** What the membership arithmetic needs from a set row on either face:
 * its name and its member ids, nothing else. */
export interface SetLike {
  name: string;
  profiles: readonly Pick<DraftRow, "profile_id">[];
}

/** The editable fields of one member row. The admin wire carries no
 * limits (budget / tpm / rpm), so the member form has none either. The
 * modalities are the tag selection: an empty selection is text only. The
 * context window stays the text the field holds, so a typo shows where it
 * was made instead of being rounded into a number the catalog takes. */
export interface MemberEdit {
  model: string;
  context_window: string;
  effort: string | null;
  modalities: Modality[];
}

/** One member's PUT body: the fields the form edits, in wire spelling.
 * `parked` is the catalog's promote flag; the shared diff never sets it,
 * and each face's port decides: the catalog adds it, a user's own
 * collection cannot send it at all. */
export interface MemberUpdateBody {
  model: string;
  max_context_window: number | null;
  effort: string | null;
  modalities: string[] | null;
  parked?: false;
}

/** One member's PUT: which row, and the body that lands on it. */
export interface MemberUpdate {
  profileId: string;
  body: MemberUpdateBody;
}

/** What the dialog hands back: the description, the order, and the rows
 * whose content changed (an untouched row needs no PUT). */
export interface SetSaveDraft {
  description: string;
  members: string[];
  updates: MemberUpdate[];
}

/** The editable projection of a member row: canonical order, and only
 * the spellings the tag group can show (a stored value outside the wire
 * set is not representable here). */
export function memberEditOf(row: DraftRow): MemberEdit {
  return {
    model: row.model,
    context_window:
      row.max_context_window === null ? "" : String(row.max_context_window),
    effort: row.effort ?? null,
    modalities: MODALITY_ORDER.filter((m) =>
      (row.modalities ?? []).includes(m),
    ),
  };
}

/** The context field as the wire takes it: null for empty (no limit), the
 * count for a positive whole number, undefined for anything else. */
export function parseContextWindow(value: string): number | null | undefined {
  const text = value.trim();
  if (text === "") return null;
  if (!/^[0-9]+$/.test(text)) return undefined;
  const limit = Number(text);
  return Number.isSafeInteger(limit) && limit > 0 ? limit : undefined;
}

/** True when a drafted row holds a window the catalog must not take: a
 * sign, a fraction, text, or zero. Empty is no limit, not a problem. */
export function contextProblem(edit: MemberEdit): boolean {
  return parseContextWindow(edit.context_window) === undefined;
}

/** True when the model field holds nothing: the server refuses an empty
 * model, and a whitespace-only one would land a row no request can serve,
 * so the draft is trimmed before it is compared or sent. */
export function modelProblem(edit: MemberEdit): boolean {
  return edit.model.trim() === "";
}

/** True when the row's draft holds a field the catalog would refuse. */
export function editProblem(edit: MemberEdit): boolean {
  return modelProblem(edit) || contextProblem(edit);
}

/** The first drafted row whose fields the server would refuse, so the
 * dialog opens that row's form instead of sending it. */
export function firstOffending<T extends { profile_id: string }>(
  members: readonly T[],
  edits: Readonly<Record<string, MemberEdit>>,
): T | undefined {
  return members.find((member) => {
    const edit = edits[member.profile_id];
    return edit !== undefined && editProblem(edit);
  });
}

/** The list with the member at `index` moved by `delta` places. A move
 * that lands outside the list, or moves nothing, answers the same list
 * back (same reference), so a disabled step and a real one are told
 * apart by identity rather than by a second bounds check at the caller. */
export function moveMember<T extends DraftRow>(
  members: readonly T[],
  index: number,
  delta: number,
): readonly T[] {
  const to = index + delta;
  if (index < 0 || index >= members.length) return members;
  if (to === index) return members;
  if (to < 0 || to >= members.length) return members;
  const next = [...members];
  const [moved] = next.splice(index, 1);
  if (moved === undefined) return members;
  next.splice(to, 0, moved);
  return next;
}

/** The order the PATCH carries (the failover sequence). */
export function memberIds(members: SetLike["profiles"]): string[] {
  return members.map((member) => member.profile_id);
}

/** The rows whose content changed, in display order. A reordered list
 * yields nothing here: order travels in `members`, content in `updates`.
 * Modality order is not a difference, because the tag group rebuilds the
 * selection in canonical order. */
export function changedUpdates<T extends DraftRow>(
  original: readonly T[],
  members: readonly T[],
  edits: Readonly<Record<string, MemberEdit>>,
): MemberUpdate[] {
  const byId = new Map(original.map((row) => [row.profile_id, row]));
  const updates: MemberUpdate[] = [];
  for (const member of members) {
    const edit = edits[member.profile_id];
    if (edit === undefined) continue;
    // Every member came out of the set's own rows, so a row missing here
    // has nothing to diff against and nothing to PUT.
    const before = byId.get(member.profile_id);
    if (before === undefined) continue;
    if (sameEdit(memberEditOf(before), edit)) continue;
    updates.push({
      profileId: member.profile_id,
      body: {
        // The server takes the trimmed id: trailing spaces are typing, not
        // the model's name.
        model: edit.model.trim(),
        max_context_window: contextWindowOf(edit),
        effort: edit.effort,
        // The tag selection as the wire takes it: an empty selection is
        // text only, which the server spells as null.
        modalities: normalizeModalities(edit.modalities)?.slice() ?? null,
      },
    });
  }
  return updates;
}

/** The catalog's PUT body: the shared fields plus the promote flag its
 * profile route requires. A user's own collection cannot send the flag, so
 * its port sends the shared body alone. */
export function adminMemberBody(
  body: MemberUpdateBody,
): MemberUpdateBody & { parked: boolean } {
  return { ...body, parked: false };
}

/** What a dialog holds after the page hands a re-read set back: the
 * member rows, their drafts, and the open form. */
export interface Reseeded<T extends DraftRow> {
  members: T[];
  edits: Record<string, MemberEdit>;
  editingId: string | null;
  /** The member ids the registry held at this re-seed. The next re-read
   * compares an order against it, so a sequence only stays the user's
   * while it differs from this one. */
  seededIds: string[];
}

/** Whether the draft's order differs from the seed's. A user who
 * moved nothing hands the registry's order back, so a save cannot carry a
 * stale sequence over someone else's move. An add or a removal changes the
 * ids, so it counts as the user's order too. */
export function orderChanged(
  seededIds: readonly string[],
  members: ReadonlyArray<{ profile_id: string }>,
): boolean {
  if (seededIds.length !== members.length) return true;
  return members.some((member, i) => member.profile_id !== seededIds[i]);
}

/** The dialog after the page hands a re-read set back. A save that
 * stopped reseeds: landed rows take the registry row, and rows whose PUT
 * never ran keep their draft. A row the registry no longer holds is
 * dropped only when the seed carried it (the server deleted it); a row the
 * user picked after the seed has no registry row yet, because only the
 * PATCH lands it, so it keeps its place and its draft. */
export function reseed<T extends DraftRow>(state: {
  /** The re-read set's rows, in the registry's order. */
  registry: readonly T[];
  /** The draft's order (the sequence the dialog shows). */
  members: readonly T[];
  edits: Readonly<Record<string, MemberEdit>>;
  /** The row whose form is open, if any. */
  editingId: string | null;
  /** The rows whose PUT landed before the save stopped. */
  landed: readonly string[];
  /** The seed the draft held before this re-seed. After a first stop it
   * is already the registry's order. */
  seededIds: readonly string[];
}): Reseeded<T> {
  const byId = new Map(state.registry.map((row) => [row.profile_id, row]));
  const seedSet = new Set(state.seededIds);
  const members = orderChanged(state.seededIds, state.members)
    ? state.members
        .map((member) => {
          const row = byId.get(member.profile_id);
          if (row !== undefined) return row;
          // Not in the registry: the server deleted a member the seed
          // carried, or the user added this one after the seed and its
          // PATCH never ran. Only the second keeps its draft.
          return seedSet.has(member.profile_id) ? undefined : member;
        })
        .filter((row): row is T => row !== undefined)
    : [...state.registry];
  const kept: Record<string, MemberEdit> = {};
  for (const [id, edit] of Object.entries(state.edits)) {
    // A landed row has nothing left to defend, and a row the draft no
    // longer shows would leave a form editing a row that is gone.
    if (state.landed.includes(id)) continue;
    if (!members.some((member) => member.profile_id === id)) continue;
    kept[id] = edit;
  }
  return {
    members,
    edits: kept,
    seededIds: memberIds(state.registry),
    // The form follows its draft: a row that lost one closes its form.
    editingId:
      state.editingId !== null && kept[state.editingId] !== undefined
        ? state.editingId
        : null,
  };
}

/** The window a PUT carries. The dialog checks every row with editProblem
 * before it sends, so a draft that gets here parses; a caller that skipped
 * that check fails loudly instead of landing a bad limit. */
function contextWindowOf(edit: MemberEdit): number | null {
  const limit = parseContextWindow(edit.context_window);
  if (limit === undefined) {
    throw new Error(
      `context window "${edit.context_window}" is not a positive whole number`,
    );
  }
  return limit;
}

function sameEdit(a: MemberEdit, b: MemberEdit): boolean {
  return (
    // Trailing spaces are typing, not a new model id.
    a.model.trim() === b.model.trim() &&
    // The values, not the typing: "128000" and " 128000 " are the same
    // limit, so neither counts as a content change.
    parseContextWindow(a.context_window) ===
      parseContextWindow(b.context_window) &&
    a.effort === b.effort &&
    sameModalities(a.modalities, b.modalities)
  );
}

function sameModalities(a: readonly string[], b: readonly string[]): boolean {
  if (a.length !== b.length) return false;
  const left = [...a].sort();
  const right = [...b].sort();
  return left.every((value, i) => value === right[i]);
}

/** The other sets of the same collection that also carry each member of
 * `target`, keyed by profile id. A row can sit in several sets, so an edit
 * to it lands in each of them; the dialog says which. */
export function setsAlsoHolding(
  target: SetLike,
  siblings: readonly SetLike[],
): Record<string, string[]> {
  const others = siblings.filter((row) => row.name !== target.name);
  const plan: Record<string, string[]> = {};
  for (const member of target.profiles) {
    const names = others
      .filter((row) =>
        row.profiles.some((p) => p.profile_id === member.profile_id),
      )
      .map((row) => row.name);
    if (names.length > 0) plan[member.profile_id] = names;
  }
  return plan;
}
