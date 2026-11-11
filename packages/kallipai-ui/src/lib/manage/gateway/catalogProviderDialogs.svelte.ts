// The catalog console's provider pool dialogs: the provider form (create and
// edit), the credential form, and the two remove confirms.
//
// A delete confirm gets its own latch by default; it shares the form's only
// where closing the form is part of the delete. The provider pair shares one
// busy/error, because a refused delete has to report where the user is
// looking; the credential pair keeps two, because its confirm replaces the
// form and closes it first. A shared pair reads as the sub-group's getters
// pointing at the form's.

import type { ProviderRow } from "./client.ts";

type GatewayAdminStore = (typeof import("./store.svelte.ts"))["gatewayStore"];

/** The store calls the provider dialogs make, plus the reason it reports. */
export type CatalogProviderDialogStore = Pick<
  GatewayAdminStore,
  | "addProvider"
  | "saveProvider"
  | "removeProvider"
  | "saveCredential"
  | "removeCredential"
  | "error"
>;

export interface CatalogProviderDialogOptions {
  /** The store slice these dialogs write through. */
  readonly store: CatalogProviderDialogStore;
}

/** A provider row as the create/edit form hands it back. */
export interface ProviderDraft {
  readonly provider_id: string;
  readonly family: string;
  readonly base_url: string | null;
  readonly api_key: string | null;
}

/** The provider form's fields: the dialog's row carries them. */
export interface ProviderFormRow {
  readonly provider_id: string;
  readonly family: string;
  readonly base_url: string | null;
}

/** The credential form's fields. */
export interface CredentialFormRow {
  readonly provider_id: string;
  readonly base_url: string | null;
}

/** The provider delete confirm. */
export interface CatalogProviderRemoveGroup {
  readonly open: boolean;
  readonly target: string | null;
  readonly busy: boolean;
  readonly error: string | null;
  ask(row: ProviderRow): void;
  confirm(): Promise<void>;
  cancel(): void;
}

/** The provider form and its delete confirm. A null row is the create form. */
export interface CatalogProviderGroup {
  readonly open: boolean;
  readonly row: ProviderFormRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  readonly remove: CatalogProviderRemoveGroup;
  show(row: ProviderFormRow | null): void;
  create(): void;
  save(result: ProviderDraft): Promise<void>;
  cancel(): void;
}

/** The credential form and its delete confirm, with their own latches. */
export interface CatalogCredentialGroup {
  readonly open: boolean;
  readonly row: CredentialFormRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  readonly remove: {
    readonly open: boolean;
    readonly target: string | null;
    readonly busy: boolean;
    readonly error: string | null;
    ask(): void;
    confirm(): Promise<void>;
    cancel(): void;
  };
  show(row: ProviderRow): void;
  save(result: { baseUrl: string; apiKey: string }): Promise<void>;
  cancel(): void;
}

/** The provider form, the credential form and their two confirms. */
export interface CatalogProviderDialogs {
  readonly provider: CatalogProviderGroup;
  readonly credential: CatalogCredentialGroup;
}

export function createCatalogProviderDialogs(
  options: CatalogProviderDialogOptions,
): CatalogProviderDialogs {
  const store = options.store;

  // -- providers ----------------------------------------------------------

  // The provider pool's dialog state. editProvider null = the create
  // form; the dialog error surfaces the store's reason (a referenced
  // provider refuses to delete with a 409, an id collision too). The edit
  // dialog and the remove confirm share one latch pair.
  let editProvider = $state<ProviderFormRow | null>(null);
  let providerDialogOpen = $state(false);
  let providerBusy = $state(false);
  let providerError = $state<string | null>(null);
  let removeProviderTarget = $state<string | null>(null);

  function openProviderDialog(row: ProviderFormRow | null): void {
    editProvider = row;
    providerError = null;
    providerDialogOpen = true;
  }

  function openProviderCreate(): void {
    openProviderDialog(null);
  }

  async function saveProvider(result: ProviderDraft): Promise<void> {
    providerBusy = true;
    providerError = null;
    if (editProvider === null) {
      const ok = await store.addProvider(result);
      providerBusy = false;
      if (!ok) {
        providerError = store.error;
        return;
      }
    } else {
      const ok = await store.saveProvider(editProvider.provider_id, {
        family: result.family,
        base_url: result.base_url,
        api_key: result.api_key,
      });
      providerBusy = false;
      if (!ok) {
        providerError = store.error;
        return;
      }
    }
    providerDialogOpen = false;
    editProvider = null;
  }

  function askRemoveProvider(row: ProviderRow): void {
    removeProviderTarget = row.provider_id;
    providerError = null;
  }

  async function confirmRemoveProvider(): Promise<void> {
    const id = removeProviderTarget;
    if (!id) return;
    providerBusy = true;
    providerError = null;
    await store.removeProvider(id);
    providerBusy = false;
    if (store.error !== null) {
      // Keep the dialog open and surface the reason (a referenced
      // provider refuses to delete with a 409).
      providerError = store.error;
    } else {
      removeProviderTarget = null;
    }
  }

  function closeProvider(): void {
    providerDialogOpen = false;
    editProvider = null;
    providerError = null;
  }

  function closeProviderRemove(): void {
    removeProviderTarget = null;
    providerError = null;
  }

  // -- credentials --------------------------------------------------------

  // The credential dialog's target and its remove confirm. They keep two
  // latch pairs: a remove refusal reports on its own confirm.
  let credentialTarget = $state<CredentialFormRow | null>(null);
  let credentialBusy = $state(false);
  let credentialError = $state<string | null>(null);
  let credentialRemoveTarget = $state<string | null>(null);
  let credentialRemoveBusy = $state(false);
  let credentialRemoveError = $state<string | null>(null);

  function openCredentialDialog(row: ProviderRow): void {
    credentialTarget = {
      provider_id: row.provider_id,
      base_url: row.base_url,
    };
    credentialError = null;
  }

  async function saveCredential(result: {
    baseUrl: string;
    apiKey: string;
  }): Promise<void> {
    if (!credentialTarget) return;
    credentialBusy = true;
    credentialError = null;
    const ok = await store.saveCredential(
      credentialTarget.provider_id,
      result.baseUrl,
      result.apiKey,
    );
    credentialBusy = false;
    if (!ok) {
      credentialError = store.error;
      return;
    }
    credentialTarget = null;
  }

  function requestCredentialRemove(): void {
    const id = credentialTarget?.provider_id;
    if (!id) return;
    credentialTarget = null;
    credentialRemoveError = null;
    credentialRemoveTarget = id;
  }

  async function confirmCredentialRemove(): Promise<void> {
    const id = credentialRemoveTarget;
    if (!id) return;
    credentialRemoveBusy = true;
    credentialRemoveError = null;
    const ok = await store.removeCredential(id);
    credentialRemoveBusy = false;
    if (!ok) {
      credentialRemoveError = store.error;
    } else {
      credentialRemoveTarget = null;
    }
  }

  function closeCredential(): void {
    credentialTarget = null;
    credentialError = null;
  }

  function closeCredentialRemove(): void {
    credentialRemoveTarget = null;
    credentialRemoveError = null;
  }

  return {
    provider: {
      get open() {
        return providerDialogOpen;
      },
      get row() {
        return editProvider;
      },
      get busy() {
        return providerBusy;
      },
      get error() {
        return providerError;
      },
      remove: {
        get open() {
          return removeProviderTarget !== null;
        },
        get target() {
          return removeProviderTarget;
        },
        // The form's pair, because the refused delete reports on the form.
        get busy() {
          return providerBusy;
        },
        get error() {
          return providerError;
        },
        ask: askRemoveProvider,
        confirm: confirmRemoveProvider,
        cancel: closeProviderRemove,
      },
      show: openProviderDialog,
      create: openProviderCreate,
      save: saveProvider,
      cancel: closeProvider,
    },
    credential: {
      get open() {
        return credentialTarget !== null;
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
      remove: {
        get open() {
          return credentialRemoveTarget !== null;
        },
        get target() {
          return credentialRemoveTarget;
        },
        get busy() {
          return credentialRemoveBusy;
        },
        get error() {
          return credentialRemoveError;
        },
        ask: requestCredentialRemove,
        confirm: confirmCredentialRemove,
        cancel: closeCredentialRemove,
      },
      show: openCredentialDialog,
      save: saveCredential,
      cancel: closeCredential,
    },
  };
}
