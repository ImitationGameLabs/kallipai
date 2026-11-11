// The catalog console's set dialogs. Each one owns its target, its busy
// latch, the store's reason for a refusal, and the handlers that open, save
// and close it. The drafts stay in the dialogs (SetDialog seeds its own form);
// nothing here holds a form.
//
// Opening a dialog has one path, and it carries a request token: the counter
// bumps first, the load compares after, so a slow loadSet cannot seed the form
// with a set the user already left (which a save would then PATCH onto the
// wrong registry entry), nor land a row in the create dialog. The token
// outlives the dialog on purpose: resetting it on close would let a read that
// answers after the close pass the compare and seed the freshly opened dialog
// with the set the user left.
//
// A delete confirm gets its own latch by default; it shares the form's only
// where closing the form is part of the delete.
//
// A stopped save keeps the dialog open, names what landed, and hands the
// re-read row back for the reseed. Landed is assigned before the row: the
// dialog's seed effect short-circuits on set === seeded, so the two
// assignments are not interchangeable.

import type { AdminSetRow } from "./client.ts";
import type { SetSaveDraft } from "../../gateway/setDraft.ts";
import { saveSetDraft, saveStoppedBanner } from "../../gateway/saveSet.ts";

type GatewayAdminStore = (typeof import("./store.svelte.ts"))["gatewayStore"];

/** The store calls the set dialogs make, plus the reason it reports. */
export type CatalogSetDialogStore = Pick<
  GatewayAdminStore,
  | "loadSet"
  | "updateMember"
  | "patchDescription"
  | "reorderSet"
  | "refreshAll"
  | "removeSet"
  | "addSet"
  | "setsIn"
  | "error"
>;

export interface CatalogSetDialogOptions {
  /** The store slice these dialogs write through. */
  readonly store: CatalogSetDialogStore;
  /** The open collection's name, or null on the list route. */
  readonly collection: () => string | null;
  /** Re-read the open collection's unassigned rows. */
  readonly reloadUnassigned: () => Promise<void>;
}

/** The set form. A null row never reaches it: creation has its own dialog. */
export interface CatalogSetEditGroup {
  readonly open: boolean;
  readonly row: AdminSetRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  readonly landed: readonly string[];
  edit(set: AdminSetRow): Promise<void>;
  save(result: SetSaveDraft): Promise<void>;
  cancel(): void;
}

/** The create dialog, which owns its draft and knows the set names in play. */
export interface CatalogSetCreateGroup {
  readonly open: boolean;
  readonly names: readonly string[];
  readonly busy: boolean;
  readonly error: string | null;
  show(): void;
  save(result: { name: string; description: string }): Promise<void>;
  cancel(): void;
}

/** The set delete confirm, with its own latch. */
export interface CatalogSetRemoveGroup {
  readonly open: boolean;
  readonly target: AdminSetRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  ask(set: AdminSetRow): void;
  confirm(): Promise<void>;
  cancel(): void;
}

/** The set form, the create dialog and the delete confirm. */
export interface CatalogSetDialogs {
  readonly edit: CatalogSetEditGroup;
  readonly create: CatalogSetCreateGroup;
  readonly remove: CatalogSetRemoveGroup;
}

export function createCatalogSetDialogs(
  options: CatalogSetDialogOptions,
): CatalogSetDialogs {
  const store = options.store;
  const reloadUnassigned = options.reloadUnassigned;

  // -- sets: edit, create, remove -----------------------------------------

  let editSet = $state<AdminSetRow | null>(null);
  let dialogOpen = $state(false);
  let setDialogBusy = $state(false);
  let setDialogError = $state<string | null>(null);
  let setDialogLanded = $state<string[]>([]);
  // The newest click's token: only its answer may seed a dialog. Pure
  // bookkeeping, so no effect should track it.
  let editRequest = 0;

  /** The one way in: the token bumps first, and a null target is the create
   * dialog, which reads no row. */
  async function open(target: AdminSetRow | null): Promise<void> {
    const request = ++editRequest;
    if (target === null) {
      setCreateError = null;
      setCreateOpen = true;
      return;
    }
    const row = await store.loadSet(target.collection_name, target.name);
    if (!row || request !== editRequest) return;
    editSet = row;
    setDialogError = null;
    setDialogBusy = false;
    setDialogLanded = [];
    dialogOpen = true;
  }

  /** The template's name for the edit path; it funnels through open. */
  function onEditSet(set: AdminSetRow): Promise<void> {
    return open(set);
  }

  // Land the dialog's draft: the member PUTs, then the description and
  // order writes, through the lib's step function so the failure paths are
  // tested without a browser. A stopped save keeps the dialog open, names
  // what landed, and hands the re-read row back for the reseed. The read
  // side belongs to the sequence (its settle port), not to any one write.
  async function onSaveSet(result: SetSaveDraft): Promise<void> {
    const target = editSet;
    if (!target) return;
    setDialogBusy = true;
    setDialogError = null;
    try {
      const outcome = await saveSetDraft(target, result, {
        putMember: async (profileId, body) => {
          const ok = await store.updateMember(
            target.collection_name,
            profileId,
            body,
          );
          return { ok, reason: ok ? null : store.error };
        },
        patchDescription: async (description) => {
          const ok = await store.patchDescription(
            target.collection_name,
            target.name,
            description,
          );
          return { ok, reason: ok ? null : store.error };
        },
        reorder: async (members) => {
          const ok = await store.reorderSet(
            target.collection_name,
            target.name,
            members,
          );
          return { ok, reason: ok ? null : store.error };
        },
        // One save spans several requests, so the read side refreshes once,
        // here, after the last write and before the re-read below.
        settle: () => store.refreshAll(),
        reread: () => store.loadSet(target.collection_name, target.name),
      });
      if (outcome.phase !== "done") {
        // The landed list lands before the re-read row: the dialog's
        // effect short-circuits on set === seeded, so these two
        // assignments are not interchangeable.
        setDialogLanded = outcome.landed;
        setDialogError = saveStoppedBanner(outcome);
        if (outcome.row) editSet = outcome.row;
        return;
      }
      dialogOpen = false;
      editSet = null;
      setDialogLanded = [];
      // Dropping a set's last member flips that profile back to
      // unassigned server-side, so a member edit moves the unassigned
      // panel too.
      await reloadUnassigned();
    } finally {
      setDialogBusy = false;
    }
  }

  function closeSetEdit(): void {
    dialogOpen = false;
    editSet = null;
    setDialogError = null;
  }

  // The create-set dialog's wiring (the dialog owns the draft).
  let setCreateOpen = $state(false);
  let setCreateBusy = $state(false);
  let setCreateError = $state<string | null>(null);

  /** The template's name for the create path; it funnels through open. */
  function openSetCreate(): void {
    void open(null);
  }

  async function saveNewSet(result: {
    name: string;
    description: string;
  }): Promise<void> {
    setCreateBusy = true;
    setCreateError = null;
    const ok = await store.addSet(
      options.collection() ?? "",
      result.name,
      result.description,
    );
    setCreateBusy = false;
    if (!ok) {
      // Keep the dialog open and surface the reason (a duplicate name
      // that slips past the front check is refused with a 409).
      setCreateError = store.error;
      return;
    }
    setCreateOpen = false;
  }

  function closeSetCreate(): void {
    setCreateOpen = false;
    setCreateError = null;
  }

  let removeTarget = $state<AdminSetRow | null>(null);
  let removeBusy = $state(false);
  let removeError = $state<string | null>(null);

  function askRemoveSet(set: AdminSetRow): void {
    removeTarget = set;
    removeError = null;
  }

  async function confirmRemove(): Promise<void> {
    const target = removeTarget;
    if (!target) return;
    removeBusy = true;
    removeError = null;
    await store.removeSet(target.collection_name, target.name);
    removeBusy = false;
    if (store.error !== null) {
      // Keep the dialog open and surface the reason (a referenced set
      // refuses to delete with a 409).
      removeError = store.error;
    } else {
      removeTarget = null;
      await reloadUnassigned();
    }
  }

  function closeRemove(): void {
    removeTarget = null;
    removeError = null;
  }

  return {
    edit: {
      get open() {
        return dialogOpen;
      },
      get row() {
        return editSet;
      },
      get busy() {
        return setDialogBusy;
      },
      get error() {
        return setDialogError;
      },
      get landed() {
        return setDialogLanded;
      },
      edit: onEditSet,
      save: onSaveSet,
      cancel: closeSetEdit,
    },
    create: {
      get open() {
        return setCreateOpen;
      },
      get names() {
        return store.setsIn(options.collection() ?? "").map((s) => s.name);
      },
      get busy() {
        return setCreateBusy;
      },
      get error() {
        return setCreateError;
      },
      show: openSetCreate,
      save: saveNewSet,
      cancel: closeSetCreate,
    },
    remove: {
      get open() {
        return removeTarget !== null;
      },
      get target() {
        return removeTarget;
      },
      get busy() {
        return removeBusy;
      },
      get error() {
        return removeError;
      },
      ask: askRemoveSet,
      confirm: confirmRemove,
      cancel: closeRemove,
    },
  };
}
