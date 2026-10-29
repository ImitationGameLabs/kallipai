// The user face's projection of its own rows onto the shared set-dialog
// arithmetic (lib/manage/gateway/setDraft.ts and saveSet.ts). Both faces
// share the draft rules; what differs is the source row type and the member
// PUT body: a user's own collection cannot carry the catalog's served flag.

import type {
  DraftRow,
  MemberUpdateBody,
  SetLike,
  SetSaveDraft,
} from "../manage/gateway/setDraft.ts";
import type { DraftSet } from "../manage/gateway/saveSet.ts";
import type { UserProfileRow, UserSetRow } from "./client.ts";

/** A set on the user face with its member rows in registry order: the
 * dialog's seed, and the row a save diffs and patches against. */
export interface UserSetDetail extends DraftSet {
  collection_name: string;
  name: string;
}

/** The user face's PUT body for one member: the shared fields alone. A
 * user's own row is parked by construction and has no promote route, so the
 * catalog's flag never rides this request. */
export interface UserMemberBody {
  model: string;
  max_context_window: number | null;
  effort: string | null;
  modalities: string[] | null;
}

/** What the user dialog hands back: the shared draft plus the name a create
 * needs (an edit keeps the registry's name). */
export interface UserSetSaveDraft extends SetSaveDraft {
  name: string;
}

/** The shared draft's view of one user profile row. A row with no modality
 * list serves text alone, which the form reads as text selected. */
export function draftRowOf(row: UserProfileRow): DraftRow {
  return {
    profile_id: row.profile_id,
    family: row.family,
    model: row.model,
    max_context_window: row.max_context_window,
    effort: row.effort,
    modalities: row.modalities,
    parked: row.parked,
  };
}

/** The set row with its members resolved to rows, in the order the wire
 * names them (the failover sequence). A member id the collection's rows no
 * longer answer is dropped: the registry holds the membership truth, and a
 * row this face cannot read cannot be edited here either.
 *
 * A null row set is not an empty one: null means the rows were never read,
 * and answers null so a caller cannot build a detail from rows it does not
 * have. Such a detail would drop every member, and the save that follows
 * would clear the set.
 */
export function setDetailFor(
  row: UserSetRow,
  profiles: readonly UserProfileRow[] | null,
): UserSetDetail | null {
  if (profiles === null) return null;
  const byId = new Map(
    profiles.map((profile) => [profile.profile_id, profile]),
  );
  const members: DraftRow[] = [];
  for (const id of row.members) {
    const profile = byId.get(id);
    if (profile !== undefined) members.push(draftRowOf(profile));
  }
  return {
    collection_name: row.collection_name,
    name: row.name,
    description: row.description,
    profiles: members,
  };
}

/** The member body as this face sends it: the four shared fields, never the
 * catalog's served flag. */
export function userMemberBody(body: MemberUpdateBody): UserMemberBody {
  return {
    model: body.model,
    max_context_window: body.max_context_window,
    effort: body.effort,
    modalities: body.modalities,
  };
}

/** The rows a save diffs against: the set's own rows plus the pool rows the
 * draft added, so a field edit on a just-added member still lands. Create
 * mode has no original, so every member is its own base row. */
export function diffBase<T extends DraftRow>(
  original: readonly T[] | null,
  members: readonly T[],
): T[] {
  const base = original === null ? [] : [...original];
  const known = new Set(base.map((row) => row.profile_id));
  for (const member of members) {
    if (known.has(member.profile_id)) continue;
    known.add(member.profile_id);
    base.push(member);
  }
  return base;
}

/** The collection's other sets as the membership note reads them: a name
 * and the member ids, nothing else. */
export function siblingSets(sets: readonly UserSetRow[]): SetLike[] {
  return sets.map((row) => ({
    name: row.name,
    profiles: row.members.map((id) => ({ profile_id: id })),
  }));
}

/** The rows a picker may offer: the collection's own rows the draft does
 * not carry yet. A set holds a profile once, and the pool keeps the
 * collection's order, so the picker reads like the list it came from. */
export function poolOf<P extends { profile_id: string }>(
  profiles: readonly P[],
  members: readonly { profile_id: string }[],
): P[] {
  return profiles.filter(
    (row) => !members.some((member) => member.profile_id === row.profile_id),
  );
}
