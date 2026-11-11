// The Profiles page's dialog controller: the public types and one factory that
// composes three family controllers.
//
// Each family owns its latches, its handle on the draft, and the handlers that
// open, save and close it. Form fields stay in the dialogs (ProviderDialog and
// ProfileDialog seed themselves on the open transition), so nothing here holds
// a field.
//
// A group holds a handle, never a copy of the target: the provider form and
// its remove confirm hold the provider object, the set-member form holds
// (setName, idx), the unassigned form holds (mode, idx), the set form holds the
// old name. A target that leaves the draft while its dialog is open keeps the
// behavior the page had before the move (see each family's header).
//
// What stays out: the commit path (apply and the dangling confirm) is the
// page's, because it calls the page's own switch and pick sends and its banner
// reads the page's unassigned-live snapshot.
//
// Admission rules for later work: a family that talks to another resource gets
// its own module; interfaces past 200 lines move out by family and shared
// options go to a shared file. One module holding several dialog groups is the
// repo norm (catalogCollectionDialogs.svelte.ts holds four at 395 lines).

import type {
  Modality,
  ProfileProvider,
  ReasoningEffort,
} from "@kallipai/kallipai-client";
import type { InlineProbeReport } from "./profiles-view.ts";
import type { ProfilesStore } from "./profiles.svelte.ts";
import type { ProfilesProbeCalls } from "./profilesProbes.svelte.ts";
import type { ManagementBackend } from "./client.ts";
import { createProfilesProfileDialogs } from "./profilesProfileDialogs.svelte.ts";
import { createProfilesProviderDialogs } from "./profilesProviderDialogs.svelte.ts";
import { createProfilesSetDialogs } from "./profilesSetDialogs.svelte.ts";

/** The store calls these dialogs make. */
export type ProfilesDialogStore = Pick<
  ProfilesStore,
  "draft" | "resetProbe" | "removeProvider" | "removeSet"
>;

/** The registry calls the set-remove confirm makes. */
export type ProfilesDialogBackend = Pick<
  ManagementBackend,
  "listAgents" | "deleteProfileSet"
>;

/** What ProviderDialog hands back on Save. */
export interface ProviderFormResult {
  id: string;
  family: string;
  baseUrl: string;
  modelListBaseUrl: string;
  apiKey: string | null;
}

/** One profile row as the set form edits it. */
export interface SetFormRow {
  id: string;
  endpoint: string;
  model: string;
  max_context_window: number;
  store?: boolean;
  effort?: ReasoningEffort;
}

/** What SetDialog hands back on Save. */
export interface SetFormResult {
  name: string;
  description: string | null;
  rows: SetFormRow[];
}

/** What ProfileDialog hands back on Save and Test. */
export interface ProfileFormResult {
  id: string;
  endpoint: string;
  model: string;
  max_context_window: number;
  store?: boolean;
  effort?: ReasoningEffort;
  modalities?: readonly Modality[];
}

export interface ProviderDialogGroup {
  readonly open: boolean;
  readonly mode: "new" | "edit";
  readonly provider: ProfileProvider | null;
  create(): void;
  show(provider: ProfileProvider): void;
  save(result: ProviderFormResult): void;
  close(): void;
}

export interface ProviderRemoveGroup {
  readonly target: ProfileProvider | null;
  ask(provider: ProfileProvider): void;
  confirm(): void;
  cancel(): void;
}

export interface SetFormGroup {
  readonly open: boolean;
  readonly name: string;
  show(name: string): void;
  save(result: SetFormResult): void;
  close(): void;
}

export interface SetRemoveGroup {
  readonly name: string | null;
  readonly users: readonly string[];
  readonly error: string | null;
  ask(name: string): Promise<void>;
  confirm(): Promise<void>;
  cancel(): void;
}

export interface SetMemberProfileGroup {
  readonly open: boolean;
  readonly setName: string;
  readonly idx: number;
  /** The in-form probe verdict this dialog owns. */
  readonly report: InlineProbeReport | null;
  show(setName: string, idx: number): void;
  save(values: ProfileFormResult): void;
  test(values: ProfileFormResult): Promise<void>;
  close(): void;
}

export interface UnassignedProfileGroup {
  readonly open: boolean;
  readonly mode: "new" | "edit";
  readonly idx: number;
  /** The in-form probe verdict this dialog owns. */
  readonly report: InlineProbeReport | null;
  create(): void;
  show(idx: number): void;
  save(values: ProfileFormResult): void;
  remove(): void;
  test(values: ProfileFormResult): Promise<void>;
  close(): void;
}

export interface ProfilesProfileDialogs {
  readonly member: SetMemberProfileGroup;
  readonly unassigned: UnassignedProfileGroup;
}

export interface ProfilesDialogs {
  readonly provider: ProviderDialogGroup;
  readonly providerRemove: ProviderRemoveGroup;
  readonly set: SetFormGroup;
  readonly setRemove: SetRemoveGroup;
  readonly profile: ProfilesProfileDialogs;
}

export interface ProfilesDialogsOptions {
  readonly store: ProfilesDialogStore;
  readonly backend: () => ProfilesDialogBackend;
  readonly probes: ProfilesProbeCalls;
  readonly refreshUnassignedLive: () => Promise<void>;
}

/** Build this page's dialog controller: one controller, three families. */
export function createProfilesDialogs(
  options: ProfilesDialogsOptions,
): ProfilesDialogs {
  const provider = createProfilesProviderDialogs(options);
  const set = createProfilesSetDialogs(options);
  return {
    provider: provider.provider,
    providerRemove: provider.providerRemove,
    set: set.set,
    setRemove: set.setRemove,
    profile: createProfilesProfileDialogs(options),
  };
}
