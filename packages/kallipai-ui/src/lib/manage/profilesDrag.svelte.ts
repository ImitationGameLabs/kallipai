// The Profiles page's drag lifecycle: which card is being dragged, which drop
// target is hovered, and what a drop does to the draft.
//
// Every drop clears the drag state first, so a failed move can never leave the
// page armed with a payload it already consumed. The report bookkeeping is
// keyed by area: a set member's report key carries its set name, an
// unassigned card's carries `p:<id>`, so a move across areas clears the old
// key and the destination re-keys on its next Test. A reorder inside one set
// keeps its key, because the set has not changed.

import {
  moveFromUnassigned,
  moveProfile,
  moveToUnassigned,
} from "../compute.ts";
import type { ProfilesProbeCalls } from "./profilesProbes.svelte.ts";

import type { ProfilesStore } from "./profiles.svelte.ts";

/** The store calls the drag paths make. */
export type ProfilesDragStore = Pick<ProfilesStore, "draft">;

export interface DragPayload {
  readonly area: "set" | "unassigned";
  readonly fromSet: string | null;
  readonly fromIdx: number;
}

export interface ProfilesDrag {
  readonly current: DragPayload | null;
  readonly overSet: string | null;
  readonly overUnassigned: boolean;
  start(payload: DragPayload): void;
  enterSet(setName: string): void;
  leaveSet(setName: string): void;
  enterUnassigned(): void;
  leaveUnassigned(): void;
  clear(): void;
  dropToSet(toSet: string): void;
  dropToUnassigned(): void;
}

export interface ProfilesDragOptions {
  readonly store: ProfilesDragStore;
  readonly probes: ProfilesProbeCalls;
  readonly refreshUnassignedLive: () => Promise<void>;
}

export function createProfilesDrag(options: ProfilesDragOptions): ProfilesDrag {
  const { store, probes } = options;

  let current = $state<DragPayload | null>(null);
  let overSet = $state<string | null>(null);
  let overUnassigned = $state(false);

  function clear(): void {
    current = null;
    overSet = null;
    overUnassigned = false;
  }

  function dropToSet(toSet: string): void {
    const payload = current;
    clear();
    const draft = store.draft;
    if (!payload || !draft) return;
    if (payload.area === "unassigned") {
      // unassigned to set: the p:-keyed report is area-scoped, clear it.
      const id = draft.parking?.[payload.fromIdx]?.id;
      if (id) probes.clearUnassignedProfile(id);
      store.draft = moveFromUnassigned(draft, payload.fromIdx, toSet);
      void options.refreshUnassignedLive();
      return;
    }
    if (payload.fromSet !== null && payload.fromSet !== toSet) {
      // Cross-set: the key is set-scoped, so clear the stale source entry
      // (same-set keeps its key, the report survives the reorder).
      const id = draft.sets[payload.fromSet]?.profiles[payload.fromIdx]?.id;
      if (id) probes.clearSetProfile(payload.fromSet, id);
    }
    if (payload.fromSet !== null) {
      store.draft = moveProfile(draft, payload.fromSet, payload.fromIdx, toSet);
    }
  }

  function dropToUnassigned(): void {
    const payload = current;
    clear();
    const draft = store.draft;
    if (!payload || !draft || payload.area !== "set") return;
    if (payload.fromSet === null) return;
    // set to unassigned: clear the set-scoped source entry; the card re-keys
    // its report as p:<id> on the next unassigned Test.
    const id = draft.sets[payload.fromSet]?.profiles[payload.fromIdx]?.id;
    if (id) probes.clearSetProfile(payload.fromSet, id);
    store.draft = moveToUnassigned(draft, payload.fromSet, payload.fromIdx);
    void options.refreshUnassignedLive();
  }

  return {
    get current() {
      return current;
    },
    get overSet() {
      return overSet;
    },
    get overUnassigned() {
      return overUnassigned;
    },
    start: (payload: DragPayload) => {
      current = payload;
    },
    enterSet: (setName: string) => {
      overSet = setName;
    },
    leaveSet: (setName: string) => {
      overSet = setName === overSet ? null : overSet;
    },
    enterUnassigned: () => {
      overUnassigned = true;
    },
    leaveUnassigned: () => {
      overUnassigned = false;
    },
    clear,
    dropToSet,
    dropToUnassigned,
  };
}
