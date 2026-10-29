// The set dialogs' multi-step save. Changed member rows land as
// their own PUTs, then the description and the member order ride one
// PATCH. The sequence lives here, behind four ports, so a test can drive
// the failure paths (which requests ran, how often the set was read back)
// without a browser, and the page only wires the ports to the store.
//
// One save spans several requests, so the read side belongs to the
// sequence, not to any single write: `settle` runs exactly once, after the
// last write and before the re-read, so the rows the re-read projects are
// the ones this save left behind.

import {
  memberIds,
  type DraftRow,
  type MemberUpdateBody,
  type SetSaveDraft,
} from "./setDraft.ts";

import {
  manage_gateway_set_save_members,
  manage_gateway_set_save_partial,
} from "../../../paraglide/messages.js";

/** One request's verdict: whether it landed, and why not when it did not. */
export interface StepResult {
  ok: boolean;
  reason: string | null;
}

/** What the save reads from a set row on either face: its description
 * and its member rows in registry order. */
export interface DraftSet {
  description: string;
  profiles: readonly DraftRow[];
}

/** The PATCH body: only the parts of the draft that differ from the row
 * the dialog was seeded with. */
export interface SetPatchBody {
  description?: string;
  members?: string[];
}

/** The requests the save makes, in the order it makes them. The re-read
 * answers the face's own detail row, so the page can hand it back to the
 * dialog it came from. */
export interface SetSavePorts<S extends DraftSet = DraftSet> {
  putMember(profileId: string, body: MemberUpdateBody): Promise<StepResult>;
  patchSet(body: SetPatchBody): Promise<StepResult>;
  /** Refresh the caller's read side, once per save, after the last write
   * and before `reread`: the re-read projects from what this call leaves
   * in the caller's cache, so it has to run first. The port reports
   * its own failure on the store it read from; the save's sequence
   * does not carry it. */
  settle(): Promise<void> | void;
  /** The set as the registry holds it after a failure. */
  reread(): Promise<S | undefined>;
}

/** What the save did: the step that stopped it (if any), the rows whose
 * PUT landed, and the re-read row after a failure. */
export interface SetSaveOutcome<S extends DraftSet = DraftSet> {
  phase: "done" | "put" | "patch";
  landed: string[];
  /** The rows whose content differed, so the rows a PUT had to reach. */
  total: number;
  /** The PATCH had something to send (a description or order change). */
  patchPending: boolean;
  reason: string | null;
  row: S | undefined;
}

/** The PATCH body for the draft, or null when the description and the
 * order both match the row: an untouched order stays off the wire, so a
 * dialog opened before someone else moved a member cannot rewrite that
 * membership on its way out. */
export function setPatchBody(
  original: DraftSet,
  draft: SetSaveDraft,
): SetPatchBody | null {
  const body: SetPatchBody = {};
  if (draft.description !== original.description) {
    body.description = draft.description;
  }
  if (!sameIds(original.profiles, draft.members)) {
    body.members = [...draft.members];
  }
  return body.description === undefined && body.members === undefined
    ? null
    : body;
}

function sameIds(
  original: readonly DraftRow[],
  members: readonly string[],
): boolean {
  const ids = memberIds(original);
  return (
    ids.length === members.length && ids.every((id, i) => id === members[i])
  );
}

/** The refresh is a step of this sequence, not a request of its own: the
 * sequence has already decided what it reports, so a refresh that throws
 * must not take the outcome away from the caller. The port that refreshed
 * is where a failure lands, on the store it read from. */
async function settleReadSide<S extends DraftSet>(
  ports: SetSavePorts<S>,
): Promise<void> {
  try {
    await ports.settle();
  } catch {
    // Swallowed on purpose: the sequence already knows what it reports,
    // and the refresh's own store carries the failure.
  }
}

/** Land the draft: each changed row as one PUT (in display order), then
 * the description and order as one PATCH. The first refusal stops the
 * save, settles the caller's read side, reads the set back once, and
 * reports what landed; nothing after the refusal runs. */
export async function saveSetDraft<S extends DraftSet>(
  original: S,
  draft: SetSaveDraft,
  ports: SetSavePorts<S>,
): Promise<SetSaveOutcome<S>> {
  const landed: string[] = [];
  for (const update of draft.updates) {
    const step = await ports.putMember(update.profileId, update.body);
    if (!step.ok) {
      await settleReadSide(ports);
      return {
        phase: "put",
        landed,
        total: draft.updates.length,
        patchPending: setPatchBody(original, draft) !== null,
        reason: step.reason,
        row: await ports.reread(),
      };
    }
    landed.push(update.profileId);
  }
  const body = setPatchBody(original, draft);
  if (body === null) {
    await settleReadSide(ports);
    return {
      phase: "done",
      landed,
      total: draft.updates.length,
      patchPending: false,
      reason: null,
      row: undefined,
    };
  }
  const step = await ports.patchSet(body);
  if (!step.ok) {
    await settleReadSide(ports);
    return {
      phase: "patch",
      landed,
      total: draft.updates.length,
      patchPending: true,
      reason: step.reason,
      row: await ports.reread(),
    };
  }
  await settleReadSide(ports);
  return {
    phase: "done",
    landed,
    total: draft.updates.length,
    patchPending: true,
    reason: null,
    row: undefined,
  };
}

/** The banner under a stopped save: what landed, and why it stopped.
 * The rows are the one part of the draft the dialog cannot show as saved,
 * so the sentence counts them. A stop with nothing landed has only the
 * server's reason; one with rows landed names the reason after the count
 * when there is one. Both faces show this same sentence. */
export function saveStoppedBanner(
  outcome: Pick<SetSaveOutcome, "landed" | "total" | "patchPending" | "reason">,
): string | null {
  if (outcome.landed.length === 0) return outcome.reason;
  const counts = { saved: outcome.landed.length, total: outcome.total };
  const saved = outcome.patchPending
    ? manage_gateway_set_save_partial(counts)
    : manage_gateway_set_save_members(counts);
  return outcome.reason === null ? saved : `${saved} ${outcome.reason}`;
}
