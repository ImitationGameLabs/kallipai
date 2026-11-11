// The Profiles page's provider dialogs: the form and its remove confirm.
// Both hold the provider object the menu handed over, so a provider that
// leaves the draft while the dialog is open still has a target; Save
// re-creates it by id through upsertProvider (pinned by a test).

import { providerFromDialog, upsertProvider } from "../compute.ts";
import type { ProfileProvider } from "@kallipai/kallipai-client";
import type {
  ProfilesDialogsOptions,
  ProviderDialogGroup,
  ProviderFormResult,
  ProviderRemoveGroup,
} from "./profilesDialogs.svelte.ts";

export interface ProfilesProviderFamily {
  readonly provider: ProviderDialogGroup;
  readonly providerRemove: ProviderRemoveGroup;
}

export function createProfilesProviderDialogs(
  options: Pick<ProfilesDialogsOptions, "store" | "probes">,
): ProfilesProviderFamily {
  const { store, probes } = options;

  let open = $state(false);
  let mode = $state<"new" | "edit">("new");
  let target = $state<ProfileProvider | null>(null);

  function create(): void {
    open = true;
    mode = "new";
    target = null;
  }

  function show(provider: ProfileProvider): void {
    open = true;
    mode = "edit";
    target = provider;
  }

  function save(result: ProviderFormResult): void {
    const draft = store.draft;
    if (!draft) return;
    store.draft = upsertProvider(
      draft,
      providerFromDialog(result, draft.endpoints[result.id]),
    );
    open = false;
  }

  let removeTarget = $state<ProfileProvider | null>(null);

  function ask(provider: ProfileProvider): void {
    // The menu only arms the confirm; the removal happens on confirm
    // (draft-level, reversible by discarding the draft).
    removeTarget = provider;
  }

  function confirm(): void {
    if (removeTarget === null) return;
    store.removeProvider(removeTarget.id);
    probes.clearProvider(removeTarget.id);
    removeTarget = null;
  }

  return {
    provider: {
      get open() {
        return open;
      },
      get mode() {
        return mode;
      },
      get provider() {
        return target;
      },
      create,
      show,
      save,
      close: () => {
        open = false;
      },
    },
    providerRemove: {
      get target() {
        return removeTarget;
      },
      ask,
      confirm,
      cancel: () => {
        removeTarget = null;
      },
    },
  };
}
