// The Profiles page's probe call sites and the card-level results they own.
//
// A probe reply is a return value the call site owns, not shared state: the
// card that triggered it renders the result, so provider replies merge into a
// provider-keyed map and profile replies into a map keyed by `<setName>|<id>`
// for set members and `p:<id>` for unassigned cards.
//
// The dialog entry points are different: probeProfileAt and probeUnassignedAt
// take the draft they must read, build the request from it, send it, and
// return the outcome. They never read the store's draft, because a dialog's
// fields are ahead of the committed draft and a Test that read the draft would
// probe stale values. The in-form report (and reportAfterProbe) belongs to the
// dialog group that made the call.
//
// Names hold that split: cardTest* reads the committed draft, probe*At takes
// the draft it is handed.

import {
  singleProfileProbeRequest,
  singleUnassignedProfileProbeRequest,
} from "../compute.ts";
import {
  clearProfileResult,
  mergeProfileScope,
  mergeProfileScopeAll,
  mergeProviderScope,
} from "./profiles-view.ts";
import { SvelteMap } from "svelte/reactivity";
import type {
  ProfileConfig,
  ProfileModelProbeReport,
  ProfileProviderProbeReport,
  ProfileProbeResponse,
} from "@kallipai/kallipai-client";
import type { ProbeOutcome } from "./profiles-view.ts";
import type { ProfilesStore } from "./profiles.svelte.ts";

/** The store calls these probe paths make. */
export type ProfilesProbeStore = Pick<
  ProfilesStore,
  "draft" | "config" | "probeProvider" | "probeSet" | "probeAll" | "probeRaw"
>;

export interface ProfilesProbes {
  readonly providerReports: SvelteMap<string, ProfileProviderProbeReport>;
  readonly profileReports: SvelteMap<string, ProfileModelProbeReport>;
  /** Card probes read the committed draft: the values the card shows. */
  cardTestProvider(id: string): Promise<void>;
  cardTestSet(setName: string): Promise<void>;
  cardTestProfile(setName: string, profileIdx: number): Promise<void>;
  cardTestUnassigned(idx: number): Promise<void>;
  cardTestAll(): Promise<void>;
  /**
   * Build and send a single-profile probe from the draft handed in. The
   * caller owns the outcome and its report.
   */
  probeProfileAt(
    setName: string,
    idx: number,
    draft: ProfileConfig,
  ): Promise<ProbeOutcome | null>;
  /**
   * Build and send a single unassigned-profile probe from the draft handed
   * in, at `idx`. The caller owns the outcome and its report.
   */
  probeUnassignedAt(
    idx: number,
    draft: ProfileConfig,
  ): Promise<ProbeOutcome | null>;
  /** Route a landed reply to the cards it names. */
  mergeProviders(response: ProfileProbeResponse): void;
  clearAll(): void;
  /** Drop the profile cards; provider cards keep their reports. */
  clearProfileCards(): void;
  clearProvider(id: string): void;
  clearSetProfile(setName: string, id: string): void;
  clearUnassignedProfile(id: string): void;
}

/** The probe calls the dialogs and the drag controller make. */
export type ProfilesProbeCalls = Pick<
  ProfilesProbes,
  | "clearProfileCards"
  | "clearProvider"
  | "clearSetProfile"
  | "clearUnassignedProfile"
  | "mergeProviders"
  | "probeProfileAt"
  | "probeUnassignedAt"
>;

export interface ProfilesProbesOptions {
  readonly store: ProfilesProbeStore;
}

export function createProfilesProbes(
  options: ProfilesProbesOptions,
): ProfilesProbes {
  const { store } = options;

  const providerReports = new SvelteMap<string, ProfileProviderProbeReport>();
  const profileReports = new SvelteMap<string, ProfileModelProbeReport>();

  function clearAll(): void {
    providerReports.clear();
    profileReports.clear();
  }

  function mergeProviders(response: ProfileProbeResponse): void {
    mergeProviderScope(providerReports, response);
  }

  async function cardTestProvider(id: string): Promise<void> {
    const outcome = await store.probeProvider(id);
    if (outcome.status === "landed") mergeProviders(outcome.response);
  }

  async function cardTestSet(setName: string): Promise<void> {
    const outcome = await store.probeSet(setName);
    if (outcome.status === "landed") {
      mergeProfileScope(setName, profileReports, outcome.response);
    }
  }

  async function cardTestProfile(
    setName: string,
    profileIdx: number,
  ): Promise<void> {
    const draft = store.draft;
    if (!draft) return;
    const outcome = await probeProfileAt(setName, profileIdx, draft);
    if (outcome === null) return;
    if (outcome.status !== "landed") return;
    mergeProviders(outcome.response);
    mergeProfileScope(setName, profileReports, outcome.response);
  }

  async function cardTestUnassigned(idx: number): Promise<void> {
    const draft = store.draft;
    if (!draft) return;
    const outcome = await probeUnassignedAt(idx, draft);
    if (outcome === null) return;
    if (outcome.status !== "landed") return;
    const resp = outcome.response;
    mergeProviders(resp);
    const p = resp.sets[0]?.profiles[0];
    if (p) {
      const id = draft.parking?.[idx]?.id ?? p.profile_id;
      profileReports.set(`p:${id}`, p);
    }
  }

  async function cardTestAll(): Promise<void> {
    const outcome = await store.probeAll();
    if (outcome.status !== "landed") return;
    mergeProviders(outcome.response);
    mergeProfileScopeAll(profileReports, outcome.response);
  }

  async function probeProfileAt(
    setName: string,
    idx: number,
    draft: ProfileConfig,
  ): Promise<ProbeOutcome | null> {
    const body = singleProfileProbeRequest(store.config, draft, setName, idx);
    if (!body) return null;
    return await store.probeRaw(body);
  }

  async function probeUnassignedAt(
    idx: number,
    draft: ProfileConfig,
  ): Promise<ProbeOutcome | null> {
    const body = singleUnassignedProfileProbeRequest(store.config, draft, idx);
    if (!body) return null;
    return await store.probeRaw(body);
  }

  return {
    providerReports,
    profileReports,
    cardTestProvider,
    cardTestSet,
    cardTestProfile,
    cardTestUnassigned,
    cardTestAll,
    probeProfileAt,
    probeUnassignedAt,
    mergeProviders,
    clearAll,
    clearProfileCards: () => {
      profileReports.clear();
    },
    clearProvider: (id: string) => {
      providerReports.delete(id);
    },
    clearSetProfile: (setName: string, id: string) => {
      clearProfileResult(profileReports, setName, id);
    },
    clearUnassignedProfile: (id: string) => {
      profileReports.delete(`p:${id}`);
    },
  };
}
