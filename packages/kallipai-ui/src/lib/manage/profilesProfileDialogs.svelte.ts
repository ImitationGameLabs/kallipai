// The Profiles page's two profile forms: a set member's and an unassigned
// profile's. Both hold a position, not a copy.
//
// The member form holds (setName, idx): a target that left the draft keeps the
// fields its dialog seeded, and Save goes through replaceSetProfile, which
// leaves an unknown set or an out-of-range index unchanged, so the save is a
// no-op that still closes. The unassigned form holds (mode, idx) and writes at
// that index on the current list, so a moved index overwrites what sits there
// now. Tests pin both behaviors.
//
// Test builds a throwaway copy, hands it to the probe module, and keeps the
// verdict: the draft is never assigned by a Test, and the reply's report is
// owned here because the dialog is the call site.

import { replaceSetProfile, replaceUnassignedProfiles } from "../compute.ts";
import { reportAfterProbe } from "./profiles-view.ts";
import type { InlineProbeReport } from "./profiles-view.ts";
import type {
  ProfilesDialogsOptions,
  ProfilesProfileDialogs,
  ProfileFormResult,
} from "./profilesDialogs.svelte.ts";

export function createProfilesProfileDialogs(
  options: Pick<
    ProfilesDialogsOptions,
    "store" | "probes" | "refreshUnassignedLive"
  >,
): ProfilesProfileDialogs {
  const { store, probes } = options;

  let memberOpen = $state(false);
  let memberSetName = $state("");
  let memberIdx = $state(0);
  let memberReport = $state<InlineProbeReport | null>(null);

  function openMember(setName: string, idx: number): void {
    memberReport = null;
    store.resetProbe();
    memberOpen = true;
    memberSetName = setName;
    memberIdx = idx;
  }

  function saveMember(values: ProfileFormResult): void {
    const draft = store.draft;
    if (!draft) return;
    store.draft = replaceSetProfile(draft, memberSetName, memberIdx, values);
    memberOpen = false;
  }

  async function testMember(values: ProfileFormResult): Promise<void> {
    const draft = store.draft;
    if (!draft) return;
    const staged = replaceSetProfile(draft, memberSetName, memberIdx, values);
    const outcome = await probes.probeProfileAt(
      memberSetName,
      memberIdx,
      staged,
    );
    if (outcome === null) return;
    if (outcome.status === "landed") probes.mergeProviders(outcome.response);
    memberReport = reportAfterProbe(outcome, memberReport);
  }

  let unassignedOpen = $state(false);
  let unassignedMode = $state<"new" | "edit">("new");
  let unassignedIdx = $state(0);
  let unassignedReport = $state<InlineProbeReport | null>(null);

  function openUnassigned(mode: "new" | "edit", idx: number): void {
    unassignedReport = null;
    store.resetProbe();
    unassignedOpen = true;
    unassignedMode = mode;
    unassignedIdx = idx;
  }

  function saveUnassigned(values: ProfileFormResult): void {
    const draft = store.draft;
    if (!draft) return;
    const list = [...(draft.parking ?? [])];
    if (unassignedMode === "new") list.push(values);
    else list[unassignedIdx] = values;
    store.draft = replaceUnassignedProfiles(draft, list);
    unassignedOpen = false;
    void options.refreshUnassignedLive();
  }

  function removeUnassigned(): void {
    const draft = store.draft;
    if (draft && unassignedMode === "edit") {
      const id = draft.parking?.[unassignedIdx]?.id;
      store.draft = replaceUnassignedProfiles(
        draft,
        (draft.parking ?? []).filter((_, i) => i !== unassignedIdx),
      );
      if (id) probes.clearUnassignedProfile(id);
    }
    void options.refreshUnassignedLive();
    unassignedOpen = false;
  }

  async function testUnassigned(values: ProfileFormResult): Promise<void> {
    const draft = store.draft;
    if (!draft) return;
    const staged = replaceUnassignedProfiles(draft, [
      ...(draft.parking ?? []),
      values,
    ]);
    const stagedIdx = (staged.parking?.length ?? 1) - 1;
    const outcome = await probes.probeUnassignedAt(stagedIdx, staged);
    if (outcome === null) return;
    if (outcome.status === "landed") probes.mergeProviders(outcome.response);
    unassignedReport = reportAfterProbe(outcome, unassignedReport);
  }

  return {
    member: {
      get open() {
        return memberOpen;
      },
      get setName() {
        return memberSetName;
      },
      get idx() {
        return memberIdx;
      },
      get report() {
        return memberReport;
      },
      show: openMember,
      save: saveMember,
      test: testMember,
      close: () => {
        memberOpen = false;
      },
    },
    unassigned: {
      get open() {
        return unassignedOpen;
      },
      get mode() {
        return unassignedMode;
      },
      get idx() {
        return unassignedIdx;
      },
      get report() {
        return unassignedReport;
      },
      create: () => openUnassigned("new", 0),
      show: (idx: number) => openUnassigned("edit", idx),
      save: saveUnassigned,
      remove: removeUnassigned,
      test: testUnassigned,
      close: () => {
        unassignedOpen = false;
      },
    },
  };
}
