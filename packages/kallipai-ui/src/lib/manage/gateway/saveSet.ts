// The set dialogs' multi-step save. A membership change is one PUT per row:
// a row that stays and changed, a row the draft adds, and a row it drops
// each carry their own write. The description and the order follow as their
// own writes, because the server takes a set's description alone and its
// order only as a permutation of the membership it holds right then.
//
// The sequence lives here, behind five ports, so a test can drive the
// failure paths (which requests ran, where the save stopped, how often the
// set was read back) without a browser, and the page only wires the ports
// to the store.

import {
  memberIds,
  orderToSend,
  type DraftRow,
  type MemberWriteBody,
  type SetSaveDraft,
} from "./setDraft.ts";

import {
  manage_gateway_set_save_description,
  manage_gateway_set_save_description_only,
  manage_gateway_set_save_members,
  manage_gateway_set_save_order,
  manage_gateway_set_save_order_only,
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

/** The requests the save makes, in the order it makes them. The re-read
 * answers the face's own detail row, so the page can hand it back to the
 * dialog it came from. */
export interface SetSavePorts<S extends DraftSet = DraftSet> {
  putMember(profileId: string, body: MemberWriteBody): Promise<StepResult>;
  patchDescription(description: string): Promise<StepResult>;
  /** The complete membership in the order it should fail over in: the server
   * refuses a list that adds or drops a member, so this runs only after every
   * membership write landed. */
  reorder(members: readonly string[]): Promise<StepResult>;
  /** Refresh the caller's read side, once per save, after the last write
   * and before `reread`: the re-read projects from what this call leaves
   * in the caller's cache, so it has to run first. The port reports
   * its own failure on the store it read from; the save's sequence
   * does not carry it. */
  settle(): Promise<void> | void;
  /** The set as the registry holds it after a failure. */
  reread(): Promise<S | undefined>;
}

/** Where a save stopped: a membership PUT, the description, the order, or
 * nowhere (`done`: the whole draft landed). */
export type SetSavePhase = "put" | "description" | "order" | "done";

/** What the save did: the step that stopped it (if any), the rows whose PUTs
 * landed, every membership request it planned, whether the draft carried a
 * description write and an order write, and the re-read row after a failure.
 * The two write flags are facts, not inferences from the step it stopped at:
 * a description stop says nothing about an order the draft never had. */
export interface SetSaveOutcome<S extends DraftSet = DraftSet> {
  phase: SetSavePhase;
  landed: string[];
  total: number;
  descriptionWrite: boolean;
  orderWrite: boolean;
  reason: string | null;
  row: S | undefined;
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

/** The outcome of a save that stopped: the read side settles, then the set is
 * read back once, so the dialog reseeds from what actually landed. */
async function stopped<S extends DraftSet>(
  ports: SetSavePorts<S>,
  phase: Exclude<SetSavePhase, "done">,
  landed: string[],
  total: number,
  rest: { description: boolean; order: boolean },
  reason: string | null,
): Promise<SetSaveOutcome<S>> {
  await settleReadSide(ports);
  return {
    phase,
    landed,
    total,
    descriptionWrite: rest.description,
    orderWrite: rest.order,
    reason,
    row: await ports.reread(),
  };
}

/** Land the draft: each membership row as one PUT (the kept edits, then the
 * joins, then the leaves), then the description and the order as their own
 * writes. The first refusal stops the save, settles the caller's read side,
 * reads the set back once, and reports what landed; nothing after the refusal
 * runs. The leaves go last because the order write needs the membership to be
 * final: `/order` takes exactly the members the set holds right then. */
export async function saveSetDraft<S extends DraftSet>(
  original: S,
  draft: SetSaveDraft,
  ports: SetSavePorts<S>,
): Promise<SetSaveOutcome<S>> {
  const writes = [...draft.updates, ...draft.joins, ...draft.leaves];
  const total = writes.length;
  const order = orderToSend(memberIds(original.profiles), draft.members);
  // What the save owes beyond the membership PUTs: a description that really
  // changed, and an order the writes left as another sequence.
  const rest = {
    description: draft.description !== original.description,
    order: order !== null,
  };
  const landed: string[] = [];
  for (const write of writes) {
    const step = await ports.putMember(write.profileId, write.body);
    if (!step.ok) {
      return stopped(ports, "put", landed, total, rest, step.reason);
    }
    landed.push(write.profileId);
  }
  if (rest.description) {
    const step = await ports.patchDescription(draft.description);
    if (!step.ok) {
      return stopped(ports, "description", landed, total, rest, step.reason);
    }
  }
  if (order !== null) {
    const step = await ports.reorder(order);
    if (!step.ok) {
      return stopped(ports, "order", landed, total, rest, step.reason);
    }
  }
  await settleReadSide(ports);
  return {
    phase: "done",
    landed,
    total,
    descriptionWrite: rest.description,
    orderWrite: rest.order,
    reason: null,
    row: undefined,
  };
}

/** The banner under a stopped save: what landed, and what it left unsent.
 * The sentence follows the step that stopped it, because every step has its
 * own truth: a save that stopped at the description already landed every
 * membership write and cannot say the members did not save, and it cannot
 * name an order write the draft never had. Both faces show this sentence. */
export function saveStoppedBanner(
  outcome: Pick<
    SetSaveOutcome,
    "phase" | "landed" | "total" | "descriptionWrite" | "orderWrite" | "reason"
  >,
): string | null {
  // Nothing landed: the reason alone is the report. A description write that
  // landed counts, so an order stop after it still names what saved.
  const descriptionLanded =
    outcome.phase === "order" && outcome.descriptionWrite;
  if (outcome.landed.length === 0 && !descriptionLanded) {
    return outcome.reason;
  }
  const counts = { saved: outcome.landed.length, total: outcome.total };
  let sentence: string;
  if (outcome.phase === "description") {
    sentence = outcome.orderWrite
      ? manage_gateway_set_save_description()
      : manage_gateway_set_save_description_only();
  } else if (outcome.phase === "order") {
    sentence = outcome.descriptionWrite
      ? manage_gateway_set_save_order()
      : manage_gateway_set_save_order_only();
  } else if (outcome.descriptionWrite || outcome.orderWrite) {
    sentence = manage_gateway_set_save_partial(counts);
  } else {
    sentence = manage_gateway_set_save_members(counts);
  }
  return outcome.reason === null ? sentence : `${sentence} ${outcome.reason}`;
}
