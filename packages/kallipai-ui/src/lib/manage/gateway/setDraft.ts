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
  /** The catalog's rows name an owner; a user's own collection has none, so
   * it is optional. `set` is the set that carries the row (null = unassigned),
   * which every face's row carries. */
  owner?: string;
  set: string | null;
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

/** One member's content in wire spelling: the fields the form edits. */
export interface MemberContentBody {
  model: string;
  max_context_window: number | null;
  effort: string | null;
  modalities: string[] | null;
}

/** A member's PUT body: the content plus the set that carries the row (null
 * leaves it unassigned). The PUT replaces the whole row, so the set is not
 * optional on the wire and no call site can leave it out. */
export interface MemberWriteBody extends MemberContentBody {
  set: string | null;
}

/** One member's PUT: which row, and the body that lands on it. */
export interface MemberUpdate {
  profileId: string;
  body: MemberWriteBody;
}

/** What the dialog hands back: the set's name, its description, the order it
 * shows, the rows whose content changed, the rows the draft adds, and the ones
 * it drops. A name lands a row at its last place, so the set the writes leave
 * behind is not always the order the dialog shows; `/order` settles that. */
export interface SetSaveDraft {
  name: string;
  description: string;
  members: string[];
  updates: MemberUpdate[];
  joins: MemberUpdate[];
  leaves: MemberUpdate[];
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

/** The content fields from a draft: the model trimmed (trailing spaces are
 * typing, not the model's name), the window parsed, the tags canonical. */
export function memberBodyOf(edit: MemberEdit): MemberContentBody {
  return {
    model: edit.model.trim(),
    max_context_window: contextWindowOf(edit),
    effort: edit.effort,
    modalities: normalizeModalities(edit.modalities)?.slice() ?? null,
  };
}

/** The four content fields a PUT carries (model, max_context_window,
 * effort, modalities), taken from a draft row or from the value a dialog
 * hands back, with the tags copied so the body cannot alias the array. */
export function contentOf(fields: {
  model: string;
  max_context_window: number | null;
  effort: string | null;
  modalities: readonly string[] | null;
}): MemberContentBody {
  return {
    model: fields.model,
    max_context_window: fields.max_context_window,
    effort: fields.effort,
    modalities: fields.modalities === null ? null : [...fields.modalities],
  };
}

/** A row's PUT body: the content it lands with, and the set it lands in. A
 * PUT replaces the whole row, so no body omits the set: it is the row's own
 * unless the caller moves the row. */
export function rowBodyOf<T extends object>(
  row: Pick<DraftRow, "set">,
  content: T,
  set: string | null = row.set,
): T & { set: string | null } {
  return { ...content, set };
}

/** The PUT for one row: the draft's edit when there is one, the row's own
 * fields otherwise, carrying the set it lands in. */
function memberWriteOf(
  row: DraftRow,
  edits: Readonly<Record<string, MemberEdit>>,
  set: string | null,
): MemberWriteBody {
  const edit = edits[row.profile_id];
  return rowBodyOf(
    row,
    edit === undefined ? contentOf(row) : memberBodyOf(edit),
    set,
  );
}

/** What a save has to PUT: the kept rows whose content changed (display
 * order), the rows the draft adds (display order), and the rows it drops (the
 * order the set held them). Every body carries its set: the draft's name for
 * the first two, null for a row on its way out. A dropped row carries the
 * draft's edit too, so editing a row and then removing it is not lost. */
export interface MembershipWrites {
  updates: MemberUpdate[];
  joins: MemberUpdate[];
  leaves: MemberUpdate[];
}

export function membershipWrites<T extends DraftRow>(
  original: readonly T[],
  members: readonly T[],
  edits: Readonly<Record<string, MemberEdit>>,
  name: string,
): MembershipWrites {
  const before = new Map(original.map((row) => [row.profile_id, row]));
  const kept = new Set(members.map((member) => member.profile_id));
  const updates: MemberUpdate[] = [];
  const joins: MemberUpdate[] = [];
  for (const member of members) {
    const row = before.get(member.profile_id);
    if (row === undefined) {
      joins.push({
        profileId: member.profile_id,
        body: memberWriteOf(member, edits, name),
      });
      continue;
    }
    const edit = edits[member.profile_id];
    // A row nobody edited, or one whose edit matches what it holds, needs no
    // PUT: an untouched row stays off the wire.
    if (edit === undefined || sameEdit(memberEditOf(row), edit)) continue;
    updates.push({
      profileId: member.profile_id,
      body: rowBodyOf(member, memberBodyOf(edit), name),
    });
  }
  const leaves: MemberUpdate[] = [];
  for (const row of original) {
    if (kept.has(row.profile_id)) continue;
    leaves.push({
      profileId: row.profile_id,
      body: memberWriteOf(row, edits, null),
    });
  }
  return { updates, joins, leaves };
}

/** The sequence `/order` should carry, or null when the membership writes
 * already leave the set in the draft's order. A name lands a row at its last
 * place, so what the writes leave behind is the members the set already had,
 * in their existing sequence, with the joins appended in draft order. */
export function orderToSend(
  originalIds: readonly string[],
  draftIds: readonly string[],
): string[] | null {
  const draft = new Set(draftIds);
  const before = new Set(originalIds);
  const after = [
    ...originalIds.filter((id) => draft.has(id)),
    ...draftIds.filter((id) => !before.has(id)),
  ];
  const same =
    after.length === draftIds.length &&
    after.every((id, index) => id === draftIds[index]);
  return same ? null : [...draftIds];
}

/** What a dialog holds after the page hands a re-read set back: the
 * member rows, their drafts, and the open form. */
interface Reseeded<T extends DraftRow> {
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
