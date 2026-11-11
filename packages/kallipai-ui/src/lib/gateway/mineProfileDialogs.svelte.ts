// The user console's profile dialog and its delete confirm. The form edit
// carries the row's collection and id: the PUT addresses the profile by
// both, and a rename is a membership move rather than a key change. The
// form and the confirm share one latch, so a refused delete keeps the
// confirm open with the store's reason.
// A delete confirm gets its own latch by default; it shares the form's only
// where closing the form is part of the delete.

import type { UserProfileRow } from "./client.ts";
import { rowBodyOf } from "./setDraft.ts";

type UserGatewayStore =
  (typeof import("./store.svelte.ts"))["userGatewayStore"];

/** The store calls the profile dialogs make, plus the reason it reports. */
export type MineProfileDialogStore = Pick<
  UserGatewayStore,
  "addProfile" | "saveProfile" | "removeProfile" | "error"
>;

export interface MineProfileDialogOptions {
  /** The store slice these dialogs write through. */
  readonly store: MineProfileDialogStore;
}

/** The form's fields: the dialog's row carries every one of them. */
export interface ProfileSaveFields {
  readonly collection: string;
  readonly profile_id: string;
  readonly provider_id: string;
  readonly model: string;
  readonly max_context_window: number | null;
  readonly effort: string | null;
  readonly modalities: string[] | null;
  readonly max_budget: number | null;
  readonly tpm_limit: number | null;
  readonly rpm_limit: number | null;
}

/** The profile delete confirm. Its latch is the form's: a refused delete has
 * to report where the user is looking. */
export interface MineProfileRemoveGroup {
  readonly open: boolean;
  readonly target: UserProfileRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  ask(row: UserProfileRow): void;
  confirm(): Promise<void>;
  cancel(): void;
}

/** The profile form and its delete confirm. */
export interface MineProfileDialogs {
  readonly open: boolean;
  readonly row: UserProfileRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  readonly remove: MineProfileRemoveGroup;
  show(row: UserProfileRow | null): void;
  save(result: ProfileSaveFields): Promise<void>;
  cancel(): void;
}

export function createMineProfileDialogs(
  options: MineProfileDialogOptions,
): MineProfileDialogs {
  const store = options.store;

  let editProfile = $state<UserProfileRow | null>(null);
  let dialogOpen = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let removeTarget = $state<UserProfileRow | null>(null);

  function show(row: UserProfileRow | null): void {
    editProfile = row;
    error = null;
    dialogOpen = true;
  }

  async function save(result: ProfileSaveFields): Promise<void> {
    busy = true;
    error = null;
    const body = {
      model: result.model,
      max_context_window: result.max_context_window,
      effort: result.effort,
      modalities: result.modalities,
      max_budget: result.max_budget,
      tpm_limit: result.tpm_limit,
      rpm_limit: result.rpm_limit,
    };
    const ok =
      editProfile === null
        ? await store.addProfile(result.collection, {
            profile_id: result.profile_id,
            provider_id: result.provider_id,
            ...body,
          })
        : await store.saveProfile(
            editProfile.collection_name,
            editProfile.profile_id,
            rowBodyOf(editProfile, body),
          );
    busy = false;
    if (!ok) {
      error = store.error;
      return;
    }
    dialogOpen = false;
    editProfile = null;
  }

  function cancel(): void {
    dialogOpen = false;
    editProfile = null;
    error = null;
  }

  function askRemove(row: UserProfileRow): void {
    removeTarget = row;
    error = null;
  }

  async function confirmRemove(): Promise<void> {
    const row = removeTarget;
    if (row === null) return;
    busy = true;
    error = null;
    await store.removeProfile(row.collection_name, row.profile_id);
    busy = false;
    if (store.error !== null) {
      // Keep the confirm open and surface the reason.
      error = store.error;
    } else {
      removeTarget = null;
    }
  }

  return {
    get open() {
      return dialogOpen;
    },
    get row() {
      return editProfile;
    },
    get busy() {
      return busy;
    },
    get error() {
      return error;
    },
    show,
    save,
    cancel,
    remove: {
      get open() {
        return removeTarget !== null;
      },
      get target() {
        return removeTarget;
      },
      get busy() {
        return busy;
      },
      get error() {
        return error;
      },
      ask: askRemove,
      confirm: confirmRemove,
      cancel: () => {
        removeTarget = null;
        error = null;
      },
    },
  };
}
