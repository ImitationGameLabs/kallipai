// The create's pre-flight. A set dialog's first save can be a create, and a
// create writes before the dialog has a set to save into -- so the rows its
// picks name have to be known first. A pick names a row of the collection
// the set lands in, and a row never read is not a row known to be absent:
// creating over unread rows would drop every pick and leave an empty set
// that looks like a successful create.
//
// The decision lives here, behind three ports, so a test can drive both
// answers -- refused before the write, or created and seeded -- without a
// browser, and the page only wires the ports to its store and its row cache.

import {
  membershipWrites,
  type DraftRow,
  type SetSaveDraft,
} from "./setDraft.ts";

import type { UserProfileRow, UserSetRow } from "./client.ts";
import { pickedRows, type UserSetDetail } from "./userSetDraft.ts";

/** What a create reads from the page: the collection's rows as the read side
 * holds them (`null` = never read), one re-read of that side, and the create
 * itself. */
export interface CreateSetPorts {
  rows(): readonly UserProfileRow[] | null;
  /** Re-read the read side. Its failure rides the page's alert, the channel
   * `addSet`'s failure uses too; this sequence carries no reason of its
   * own. */
  refresh(): Promise<void>;
  /** Register an empty set under the draft's name. `undefined` = the write
   * failed. */
  addSet(name: string, description: string): Promise<UserSetRow | undefined>;
}

/** What a create leaves the save sequence: the set row to walk (no members
 * yet -- the member PUTs add them) and the draft reseeded with the server's
 * name and the picks' rows. */
export interface CreatedSetStart {
  target: UserSetDetail;
  draft: SetSaveDraft;
}

/** The create's pre-flight, before the set exists. An empty draft needs no
 * rows and creates straight away. A draft with picks resolves them against
 * the collection's rows, reading that side once when it holds none.
 *
 * `undefined` answers both ways a create can stop -- the rows are still
 * unknown, or the write itself failed -- and the caller takes the reason
 * from the store both ports report to. Either way no set is created over
 * rows the draft could not resolve, so a refused save leaves no half-built
 * set behind. */
export async function createSetDraft(
  draft: SetSaveDraft,
  ports: CreateSetPorts,
): Promise<CreatedSetStart | undefined> {
  let picked: DraftRow[] = [];
  if (draft.members.length > 0) {
    let source = ports.rows();
    if (source === null) {
      await ports.refresh();
      source = ports.rows();
    }
    if (source === null) return undefined;
    picked = pickedRows(draft.members, source);
  }
  const created = await ports.addSet(draft.name, draft.description);
  if (created === undefined) return undefined;
  return {
    target: {
      collection_name: created.collection_name,
      name: created.name,
      description: created.description,
      profiles: [],
    },
    draft: {
      ...draft,
      name: created.name,
      ...membershipWrites([], picked, {}, created.name),
    },
  };
}
