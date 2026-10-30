// The user face's projection of its own rows onto the shared set-dialog
// arithmetic (lib/manage/gateway/setDraft.ts and saveSet.ts). Both faces
// share the draft rules; the row type differs, and a user's row names
// the set that carries it.

import type { DraftRow } from "../manage/gateway/setDraft.ts";
import type { DraftSet } from "../manage/gateway/saveSet.ts";
import type { UserProfileRow, UserSetRow } from "./client.ts";

/** A set on the user face with its member rows in registry order: the
 * dialog's seed, and the row a save diffs and patches against. */
export interface UserSetDetail extends DraftSet {
  collection_name: string;
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
    set: row.set,
  };
}

/** The rows a draft's picks name, in the draft's order, as shared draft rows.
 * A pick the collection's rows do not answer is left out: the row is gone
 * server-side, and its PUT would 404 and sink the whole save, so the pick is
 * not written. That is a judgment, not a measurement: the alternative is a
 * save that fails after the set already exists. */
export function pickedRows(
  members: readonly string[],
  source: readonly UserProfileRow[],
): DraftRow[] {
  const byId = new Map(source.map((row) => [row.profile_id, row]));
  const rows: DraftRow[] = [];
  for (const id of members) {
    const row = byId.get(id);
    if (row !== undefined) rows.push(draftRowOf(row));
  }
  return rows;
}

/** The set row with its members resolved to rows, in the order the wire
 * names them (the failover sequence). Members resolve through
 * `pickedRows`, so an id the collection's rows no longer answer is
 * dropped: a row this face cannot read is not written.
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
  const members = pickedRows(row.members, profiles);
  return {
    collection_name: row.collection_name,
    name: row.name,
    description: row.description,
    profiles: members,
  };
}

/** The rows a picker may offer: the collection's own rows the draft does
 * not carry yet. A profile belongs to one set at a time, so a row that
 * another set holds is offered too: adding it moves it. The pool keeps
 * collection's order, so the picker reads like the list it came from. */
export function poolOf<P extends { profile_id: string }>(
  profiles: readonly P[],
  members: readonly { profile_id: string }[],
): P[] {
  return profiles.filter(
    (row) => !members.some((member) => member.profile_id === row.profile_id),
  );
}
