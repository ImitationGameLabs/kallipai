// The user console's collection dialogs: the description/sets edit, the
// publish (or unpublish) dialog and the delete confirm.
//
// Opening the form has one path, and it carries a request token: the click
// bumps it, the load compares against it, and only the newest answer may seed
// the dialog. The create form is that same path with a null target, so a read
// still in flight cannot land its row in it. A slow load answering after a
// later click would repaint a closed dialog's row over the one the user
// opened, and a save would then PATCH the row the user left.
//
// The edit's save closes first and writes after: the collection dialog owns
// no busy latch and no error surface, so the write rides the store's own
// refresh and a failure surfaces on the page alert.
//
// The delete confirm quotes the row's counts: both are the server's numbers,
// so the copy names what the cascade will take.
// A delete confirm gets its own latch by default; it shares the form's only
// where closing the form is part of the delete.

import type { UserCollectionRow } from "./client.ts";
import { user_gateway_remove_collection_body } from "../../paraglide/messages.js";

type UserGatewayStore =
  (typeof import("./store.svelte.ts"))["userGatewayStore"];

/** The store calls the collection dialogs make, plus the reason it reports. */
export type MineCollectionDialogStore = Pick<
  UserGatewayStore,
  | "loadCollection"
  | "saveCollection"
  | "publish"
  | "unpublish"
  | "removeCollection"
  | "error"
>;

export interface MineCollectionDialogOptions {
  /** The store slice these dialogs write through. */
  readonly store: MineCollectionDialogStore;
}

/** The description/sets form; a null row is the create form. */
export interface MineCollectionEditGroup {
  readonly open: boolean;
  readonly row: UserCollectionRow | null;
  show(row: UserCollectionRow | null): Promise<void>;
  save(result: { description: string; sets: string[] }): void;
  cancel(): void;
}

/** The publish dialog: the reserved audience is its built-in option. */
export interface MineCollectionPublishGroup {
  readonly open: boolean;
  readonly target: UserCollectionRow | null;
  readonly mode: "publish" | "unpublish";
  readonly busy: boolean;
  readonly error: string | null;
  show(row: UserCollectionRow, mode: "publish" | "unpublish"): void;
  confirm(groupId: string): Promise<void>;
  cancel(): void;
}

/** The collection delete confirm, whose copy quotes the cascade counts. */
export interface MineCollectionRemoveGroup {
  readonly open: boolean;
  readonly target: UserCollectionRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  readonly body: string;
  ask(row: UserCollectionRow): void;
  confirm(): Promise<void>;
  cancel(): void;
}

/** The collection form, its publish dialog and its delete confirm. */
export interface MineCollectionDialogs {
  readonly edit: MineCollectionEditGroup;
  readonly publish: MineCollectionPublishGroup;
  readonly remove: MineCollectionRemoveGroup;
}

export function createMineCollectionDialogs(
  options: MineCollectionDialogOptions,
): MineCollectionDialogs {
  const store = options.store;

  // -- the edit ------------------------------------------------------------

  let editCollection = $state<UserCollectionRow | null>(null);
  let dialogOpen = $state(false);
  // Pure bookkeeping: no effect should track this counter.
  let editRequest = 0;
  let removeTarget = $state<UserCollectionRow | null>(null);
  let removeBusy = $state(false);
  let removeError = $state<string | null>(null);

  async function show(row: UserCollectionRow | null): Promise<void> {
    const request = ++editRequest;
    if (row === null) {
      // The create form reads no row; the bump above is what keeps a read
      // still in flight from seeding it.
      editCollection = null;
      dialogOpen = true;
      return;
    }
    const full = await store.loadCollection(row.name);
    if (full && request === editRequest) {
      editCollection = full;
      dialogOpen = true;
    }
  }

  function save(result: { description: string; sets: string[] }): void {
    const name = editCollection?.name;
    dialogOpen = false;
    editCollection = null;
    if (name) {
      void store.saveCollection(name, {
        description: result.description,
        sets: result.sets,
      });
    }
  }

  function cancel(): void {
    dialogOpen = false;
    editCollection = null;
  }

  // -- the publish ---------------------------------------------------------

  // Mode picks publish or unpublish; both answer through the store and
  // surface the server's reason (an already published collection conflicts
  // with a 409).
  let publishTarget = $state<UserCollectionRow | null>(null);
  let publishMode = $state<"publish" | "unpublish">("publish");
  let publishOpen = $state(false);
  let publishBusy = $state(false);
  let publishError = $state<string | null>(null);

  function showPublish(
    row: UserCollectionRow,
    mode: "publish" | "unpublish",
  ): void {
    publishTarget = row;
    publishMode = mode;
    publishError = null;
    publishOpen = true;
  }

  async function confirmPublish(groupId: string): Promise<void> {
    const name = publishTarget?.name;
    if (!name) return;
    publishBusy = true;
    publishError = null;
    const ok =
      publishMode === "publish"
        ? await store.publish(name, groupId)
        : await store.unpublish(name, groupId);
    publishBusy = false;
    if (!ok) {
      publishError = store.error;
      return;
    }
    publishOpen = false;
    publishTarget = null;
  }

  function cancelPublish(): void {
    publishOpen = false;
    publishTarget = null;
    publishError = null;
  }

  // -- the remove ----------------------------------------------------------

  function askRemove(row: UserCollectionRow): void {
    removeTarget = row;
    removeError = null;
  }

  async function confirmRemove(): Promise<void> {
    const name = removeTarget?.name;
    if (!name) return;
    removeBusy = true;
    removeError = null;
    await store.removeCollection(name);
    removeBusy = false;
    if (store.error !== null) {
      removeError = store.error;
    } else {
      removeTarget = null;
    }
  }

  /** The delete confirm's copy, built from the row. */
  function removeBody(): string {
    const row = removeTarget;
    if (row === null) return "";
    return user_gateway_remove_collection_body({
      name: row.name,
      sets: row.set_count,
      profiles: row.profile_count,
    });
  }

  return {
    edit: {
      get open() {
        return dialogOpen;
      },
      get row() {
        return editCollection;
      },
      show,
      save,
      cancel,
    },
    publish: {
      get open() {
        return publishOpen;
      },
      get target() {
        return publishTarget;
      },
      get mode() {
        return publishMode;
      },
      get busy() {
        return publishBusy;
      },
      get error() {
        return publishError;
      },
      show: showPublish,
      confirm: confirmPublish,
      cancel: cancelPublish,
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
      get body() {
        return removeBody();
      },
      ask: askRemove,
      confirm: confirmRemove,
      cancel: () => {
        removeTarget = null;
        removeError = null;
      },
    },
  };
}
