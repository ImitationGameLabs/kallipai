// The Profiles page's set dialogs: the form and its remove confirm.
//
// The form holds the name the set had when it opened, and saves through
// rename-then-description-then-rows because each helper addresses the set by
// its name at that step; a set renamed elsewhere meanwhile leaves the config
// unchanged (compute.ts) and the save still closes. The remove confirm asks
// the registry which agents still bind the set, deletes it server-side with
// force when any do, and treats a 404 as "already gone" so the local removal
// still runs.
// A rename or a removal drops the profile cards, whose keys carry the set name.
// Provider cards are untouched.

import {
  renameSet,
  replaceSetProfiles,
  updateSetDescription,
} from "../compute.ts";
import { displayError } from "./errors.ts";
import { KallipaiError } from "@kallipai/kallipai-common";
import { manage_profiles_remove_set_failed } from "../../paraglide/messages.js";
import type {
  ProfilesDialogsOptions,
  SetFormGroup,
  SetFormResult,
  SetRemoveGroup,
} from "./profilesDialogs.svelte.ts";

export interface ProfilesSetFamily {
  readonly set: SetFormGroup;
  readonly setRemove: SetRemoveGroup;
}

export function createProfilesSetDialogs(
  options: Pick<ProfilesDialogsOptions, "store" | "backend" | "probes">,
): ProfilesSetFamily {
  const { store, backend, probes } = options;

  let open = $state(false);
  let oldName = $state("");

  function save(result: SetFormResult): void {
    const draft = store.draft;
    if (!draft) return;
    const renamed = renameSet(draft, oldName, result.name);
    const described = updateSetDescription(
      renamed,
      result.name,
      result.description,
    );
    store.draft = replaceSetProfiles(described, result.name, result.rows);
    if (result.name !== oldName) probes.clearProfileCards();
    open = false;
  }

  let removeName = $state<string | null>(null);
  let removeUsers = $state<string[]>([]);
  let removeError = $state<string | null>(null);

  async function ask(name: string): Promise<void> {
    removeName = name;
    removeUsers = [];
    removeError = null;
    try {
      const { agents } = await backend().listAgents();
      removeUsers = agents
        .filter((a) => a.profile_set === name)
        .map((a) => a.role || a.id);
    } catch {
      // The user list is advisory; the delete itself surfaces failures.
    }
  }

  async function confirm(): Promise<void> {
    const name = removeName;
    if (name === null) return;
    removeError = null;
    if (removeUsers.length > 0) {
      try {
        await backend().deleteProfileSet(name, true);
      } catch (e) {
        // 404 = the set is already gone server-side (dangling users); the
        // local removal is still right. Everything else surfaces.
        if (!(e instanceof KallipaiError && e.api.status === 404)) {
          removeError = displayError(
            "profiles",
            e,
            manage_profiles_remove_set_failed(),
          );
          return;
        }
      }
    }
    store.removeSet(name);
    probes.clearProfileCards();
    removeName = null;
    removeUsers = [];
  }

  return {
    set: {
      get open() {
        return open;
      },
      get name() {
        return oldName;
      },
      show: (name: string) => {
        oldName = name;
        open = true;
      },
      save,
      close: () => {
        open = false;
      },
    },
    setRemove: {
      get name() {
        return removeName;
      },
      get users() {
        return removeUsers;
      },
      get error() {
        return removeError;
      },
      ask,
      confirm,
      cancel: () => {
        removeName = null;
        removeUsers = [];
        removeError = null;
      },
    },
  };
}
