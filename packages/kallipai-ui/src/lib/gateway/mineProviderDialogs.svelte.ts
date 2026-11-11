// The user console's provider and credential dialogs. Each one owns its
// target, its busy latch, the store's reason for a refusal, and the handlers
// that open, save and close it.
//
// A provider form and its delete confirm share one latch: a refused delete
// (a referenced provider answers 409) keeps the confirm open with the reason,
// the way a refused save keeps the form open. The credential pair shares
// theirs the same way.
// A delete confirm gets its own latch by default; it shares the form's only
// where closing the form is part of the delete.

import type { UserProviderRow } from "./client.ts";

type UserGatewayStore =
  (typeof import("./store.svelte.ts"))["userGatewayStore"];

/** The store calls the provider dialogs make, plus the reason it reports. */
export type MineProviderDialogStore = Pick<
  UserGatewayStore,
  | "addProvider"
  | "saveProvider"
  | "removeProvider"
  | "saveCredential"
  | "removeCredential"
  | "error"
>;

export interface MineProviderDialogOptions {
  /** The store slice these dialogs write through. */
  readonly store: MineProviderDialogStore;
}

/** The provider form's fields: what the dialog's row carries. */
export interface ProviderEditRow {
  readonly provider_id: string;
  readonly family: string;
  readonly base_url: string | null;
}

/** The provider delete confirm. Its latch is the form's: a refused delete has
 * to report where the user is looking. */
export interface MineProviderRemoveGroup {
  readonly open: boolean;
  readonly target: string | null;
  readonly busy: boolean;
  readonly error: string | null;
  ask(id: string): void;
  confirm(): Promise<void>;
  cancel(): void;
}

/** The provider form and its delete confirm. A null row is the create form. */
export interface MineProviderGroup {
  readonly open: boolean;
  readonly row: ProviderEditRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  readonly remove: MineProviderRemoveGroup;
  show(row: ProviderEditRow | null): void;
  save(result: ProviderEditRow & { api_key: string | null }): Promise<void>;
  cancel(): void;
}

/** The credential confirm, which has no target of its own: the remove reads
 * the form's row and closes it first. */
export interface MineCredentialRemoveGroup {
  readonly open: boolean;
  readonly busy: boolean;
  readonly error: string | null;
  ask(): void;
  confirm(): Promise<void>;
  cancel(): void;
}

/** The credential form and its delete confirm. */
export interface MineCredentialGroup {
  readonly open: boolean;
  readonly row: UserProviderRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  readonly remove: MineCredentialRemoveGroup;
  show(row: UserProviderRow): void;
  save(result: { baseUrl: string; apiKey: string }): Promise<void>;
  cancel(): void;
}

/** The provider dialog group and the credential one. */
export interface MineProviderDialogs {
  readonly provider: MineProviderGroup;
  readonly credential: MineCredentialGroup;
}

export function createMineProviderDialogs(
  options: MineProviderDialogOptions,
): MineProviderDialogs {
  const store = options.store;

  // -- providers ----------------------------------------------------------

  // Null is the create form; the error surfaces the store's reason.
  let editProvider = $state<ProviderEditRow | null>(null);
  let dialogOpen = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let removeTarget = $state<string | null>(null);

  function show(row: ProviderEditRow | null): void {
    editProvider = row;
    error = null;
    dialogOpen = true;
  }

  async function save(
    result: ProviderEditRow & { api_key: string | null },
  ): Promise<void> {
    busy = true;
    error = null;
    if (editProvider === null) {
      const ok = await store.addProvider(result);
      busy = false;
      if (!ok) {
        error = store.error;
        return;
      }
    } else {
      const ok = await store.saveProvider(editProvider.provider_id, {
        family: result.family,
        base_url: result.base_url,
        api_key: result.api_key,
      });
      busy = false;
      if (!ok) {
        error = store.error;
        return;
      }
    }
    dialogOpen = false;
    editProvider = null;
  }

  function cancel(): void {
    dialogOpen = false;
    editProvider = null;
    error = null;
  }

  function askRemove(id: string): void {
    removeTarget = id;
    error = null;
  }

  async function confirmRemove(): Promise<void> {
    const id = removeTarget;
    if (!id) return;
    busy = true;
    error = null;
    await store.removeProvider(id);
    busy = false;
    if (store.error !== null) {
      // Keep the dialog open and surface the reason (a referenced
      // provider refuses to delete with a 409).
      error = store.error;
    } else {
      removeTarget = null;
    }
  }

  // -- credential ---------------------------------------------------------

  let credentialTarget = $state<UserProviderRow | null>(null);
  let credentialOpen = $state(false);
  let credentialBusy = $state(false);
  let credentialError = $state<string | null>(null);
  let credentialConfirmOpen = $state(false);

  function showCredential(row: UserProviderRow): void {
    credentialTarget = row;
    credentialError = null;
    credentialOpen = true;
  }

  async function saveCredential(result: {
    baseUrl: string;
    apiKey: string;
  }): Promise<void> {
    const id = credentialTarget?.provider_id;
    if (!id) return;
    credentialBusy = true;
    credentialError = null;
    const ok = await store.saveCredential(id, result.baseUrl, result.apiKey);
    credentialBusy = false;
    if (!ok) {
      credentialError = store.error;
      return;
    }
    credentialOpen = false;
    credentialTarget = null;
  }

  function cancelCredential(): void {
    credentialOpen = false;
    credentialTarget = null;
    credentialError = null;
  }

  async function confirmRemoveCredential(): Promise<void> {
    const id = credentialTarget?.provider_id;
    if (!id) return;
    credentialBusy = true;
    credentialError = null;
    const ok = await store.removeCredential(id);
    credentialBusy = false;
    if (!ok) {
      credentialError = store.error;
      return;
    }
    credentialOpen = false;
    credentialTarget = null;
    credentialConfirmOpen = false;
  }

  return {
    provider: {
      get open() {
        return dialogOpen;
      },
      get row() {
        return editProvider;
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
    },
    credential: {
      get open() {
        return credentialOpen;
      },
      get row() {
        return credentialTarget;
      },
      get busy() {
        return credentialBusy;
      },
      get error() {
        return credentialError;
      },
      show: showCredential,
      save: saveCredential,
      cancel: cancelCredential,
      remove: {
        get open() {
          return credentialConfirmOpen;
        },
        get busy() {
          return credentialBusy;
        },
        get error() {
          return credentialError;
        },
        ask: () => {
          credentialConfirmOpen = true;
        },
        confirm: confirmRemoveCredential,
        cancel: () => {
          credentialConfirmOpen = false;
          credentialError = null;
        },
      },
    },
  };
}
