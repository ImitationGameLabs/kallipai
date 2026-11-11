// The catalog console's collection and group dialogs.
//
// The collection delete takes its follow-up as a parameter (afterRemove): the
// detail's subject is gone once the delete lands, but where that leaves the
// route is the caller's knowledge, not this module's.
//
// The collection row's action menu speaks one string per item, and the
// prefix protocol is this module's: the page hands the string over whole.
//
// A delete confirm gets its own latch by default; it shares the form's only
// where closing the form is part of the delete.
//
// The group dialog renders a live row. Member ops re-read the whole list on
// each landing and the store swaps the array, so the row is resolved by id on
// every read instead of holding the row captured at the click.

import type { AdminCollectionRow, GroupRow } from "./client.ts";
import { manage_gateway_remove_collection_body } from "../../../paraglide/messages.js";

type GatewayAdminStore = (typeof import("./store.svelte.ts"))["gatewayStore"];

/** The store calls the collection and group dialogs make. */
export type CatalogCollectionDialogStore = Pick<
  GatewayAdminStore,
  | "saveCollection"
  | "addCollection"
  | "removeCollection"
  | "saveGroup"
  | "addGroup"
  | "addMember"
  | "removeMember"
  | "removeGroup"
  | "groups"
  | "publishToGroup"
  | "unpublishFromGroup"
  | "error"
>;

export interface CatalogCollectionDialogOptions {
  /** The store slice these dialogs write through. */
  readonly store: CatalogCollectionDialogStore;
  /** Runs after a collection delete lands, with its row already cleared. */
  readonly afterRemove: () => void;
}

/** The collection form, its delete confirm and the row's action menu. */
export interface CatalogCollectionGroup {
  readonly open: boolean;
  readonly row: AdminCollectionRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  readonly remove: {
    readonly open: boolean;
    readonly target: AdminCollectionRow | null;
    readonly busy: boolean;
    readonly error: string | null;
    readonly body: string;
    ask(row: AdminCollectionRow): void;
    confirm(): Promise<void>;
    cancel(): void;
  };
  create(): void;
  show(row: AdminCollectionRow | null): void;
  save(result: { name: string; description: string }): Promise<void>;
  applyAction(row: AdminCollectionRow, value: string): void;
  cancel(): void;
}

/** The group form and its delete confirm. */
export interface CatalogGroupGroup {
  readonly open: boolean;
  readonly row: GroupRow | null;
  readonly busy: boolean;
  readonly memberBusy: boolean;
  readonly error: string | null;
  readonly remove: {
    readonly open: boolean;
    readonly target: GroupRow | null;
    readonly busy: boolean;
    readonly error: string | null;
    ask(row: GroupRow): void;
    confirm(): Promise<void>;
    cancel(): void;
  };
  create(): void;
  show(row: GroupRow | null): void;
  save(result: { name: string }): Promise<void>;
  addMember(account: string): Promise<void>;
  removeMember(account: string): Promise<void>;
  cancel(): void;
}

/** The collection dialogs and the group ones. */
export interface CatalogCollectionDialogs {
  readonly collection: CatalogCollectionGroup;
  readonly group: CatalogGroupGroup;
}

export function createCatalogCollectionDialogs(
  options: CatalogCollectionDialogOptions,
): CatalogCollectionDialogs {
  const store = options.store;

  // -- collections --------------------------------------------------------

  // The collection dialog's wiring: edit carries the list row (the
  // list view already holds the full shape), create opens the blank
  // form; save branches on which shape opened it.
  let editCollection = $state<AdminCollectionRow | null>(null);
  let collectionDialogOpen = $state(false);
  let collectionBusy = $state(false);
  let collectionError = $state<string | null>(null);
  let removeCollectionTarget = $state<AdminCollectionRow | null>(null);
  let removeCollectionBusy = $state(false);
  let removeCollectionError = $state<string | null>(null);

  function openCollectionDialog(row: AdminCollectionRow | null): void {
    editCollection = row;
    collectionError = null;
    collectionDialogOpen = true;
  }

  function openCollectionCreate(): void {
    openCollectionDialog(null);
  }

  async function saveCollection(result: {
    name: string;
    description: string;
  }): Promise<void> {
    collectionBusy = true;
    collectionError = null;
    const target = editCollection?.name ?? null;
    const ok =
      target !== null
        ? await store.saveCollection(target, {
            description: result.description,
          })
        : await store.addCollection(result);
    collectionBusy = false;
    if (!ok) {
      // The PATCH was refused; keep the dialog open and surface the
      // reason (a name collision answers with a 409). A refused
      // refresh does not land here: it rides the page-level alert.
      collectionError = store.error;
      return;
    }
    collectionDialogOpen = false;
    editCollection = null;
  }

  function askRemoveCollection(row: AdminCollectionRow): void {
    removeCollectionTarget = row;
    removeCollectionError = null;
  }

  async function confirmRemoveCollection(): Promise<void> {
    const name = removeCollectionTarget?.name;
    if (!name) return;
    removeCollectionBusy = true;
    removeCollectionError = null;
    await store.removeCollection(name);
    removeCollectionBusy = false;
    if (store.error !== null) {
      removeCollectionError = store.error;
    } else {
      removeCollectionTarget = null;
      // The detail's subject is gone; the caller decides where that leaves
      // the route.
      options.afterRemove();
    }
  }

  /** The row's action menu, parsed here: "publish:<id>", "unpublish:<id>",
   * "edit" and "remove" are this module's protocol, not the template's. */
  function applyAction(row: AdminCollectionRow, value: string): void {
    if (value.startsWith("publish:")) {
      void store.publishToGroup(row.name, value.slice("publish:".length));
      return;
    }
    if (value.startsWith("unpublish:")) {
      void store.unpublishFromGroup(row.name, value.slice("unpublish:".length));
      return;
    }
    if (value === "edit") {
      openCollectionDialog(row);
      return;
    }
    if (value === "remove") askRemoveCollection(row);
  }

  function closeCollection(): void {
    collectionDialogOpen = false;
    editCollection = null;
    collectionError = null;
  }

  function closeCollectionRemove(): void {
    removeCollectionTarget = null;
    removeCollectionError = null;
  }

  // -- groups -------------------------------------------------------------

  let editGroup = $state<GroupRow | null>(null);
  let groupDialogOpen = $state(false);
  let groupBusy = $state(false);
  let memberBusy = $state(false);
  let groupError = $state<string | null>(null);
  let removeGroupTarget = $state<GroupRow | null>(null);
  let removeGroupBusy = $state(false);
  let removeGroupError = $state<string | null>(null);

  // The dialog renders the live row: member ops re-read the whole
  // list on each landing and the store swaps the array, so resolve
  // by id instead of holding the row captured at the click.
  function editGroupLive(): GroupRow | null {
    if (editGroup === null) return null;
    const id = editGroup.group_id;
    return store.groups?.find((g) => g.group_id === id) ?? editGroup;
  }

  function openGroupDialog(row: GroupRow | null): void {
    editGroup = row;
    groupError = null;
    groupDialogOpen = true;
  }

  function openGroupCreate(): void {
    openGroupDialog(null);
  }

  async function saveGroup(result: { name: string }): Promise<void> {
    groupBusy = true;
    groupError = null;
    const target = editGroup?.group_id ?? null;
    const ok =
      target !== null
        ? await store.saveGroup(target, { name: result.name })
        : await store.addGroup({ name: result.name });
    groupBusy = false;
    if (!ok) {
      groupError = store.error;
      return;
    }
    groupDialogOpen = false;
    editGroup = null;
  }

  // Member ops gate on their own busy flag and surface the refusal
  // in the dialog: the page-level alert sits behind the modal
  // backdrop, so a failed add or remove would otherwise vanish.
  async function addMember(account: string): Promise<void> {
    const groupId = editGroup?.group_id;
    if (!groupId || memberBusy) return;
    memberBusy = true;
    groupError = null;
    await store.addMember(groupId, account);
    memberBusy = false;
    if (store.error !== null) {
      groupError = store.error;
    }
  }

  async function removeMember(account: string): Promise<void> {
    const groupId = editGroup?.group_id;
    if (!groupId || memberBusy) return;
    memberBusy = true;
    groupError = null;
    await store.removeMember(groupId, account);
    memberBusy = false;
    if (store.error !== null) {
      groupError = store.error;
    }
  }

  function askRemoveGroup(row: GroupRow): void {
    removeGroupTarget = row;
    removeGroupError = null;
  }

  async function confirmRemoveGroup(): Promise<void> {
    const groupId = removeGroupTarget?.group_id;
    if (!groupId) return;
    removeGroupBusy = true;
    removeGroupError = null;
    await store.removeGroup(groupId);
    removeGroupBusy = false;
    if (store.error !== null) {
      removeGroupError = store.error;
    } else {
      removeGroupTarget = null;
    }
  }

  function closeGroup(): void {
    groupDialogOpen = false;
    editGroup = null;
    groupError = null;
  }

  function closeGroupRemove(): void {
    removeGroupTarget = null;
    removeGroupError = null;
  }

  return {
    collection: {
      get open() {
        return collectionDialogOpen;
      },
      get row() {
        return editCollection;
      },
      get busy() {
        return collectionBusy;
      },
      get error() {
        return collectionError;
      },
      remove: {
        get open() {
          return removeCollectionTarget !== null;
        },
        get target() {
          return removeCollectionTarget;
        },
        get busy() {
          return removeCollectionBusy;
        },
        get error() {
          return removeCollectionError;
        },
        get body() {
          const target = removeCollectionTarget;
          return target === null
            ? ""
            : manage_gateway_remove_collection_body({
                name: target.name,
                sets: target.set_count,
                profiles: target.profile_count,
              });
        },
        ask: askRemoveCollection,
        confirm: confirmRemoveCollection,
        cancel: closeCollectionRemove,
      },
      create: openCollectionCreate,
      show: openCollectionDialog,
      save: saveCollection,
      applyAction,
      cancel: closeCollection,
    },
    group: {
      get open() {
        return groupDialogOpen;
      },
      get row() {
        return editGroupLive();
      },
      get busy() {
        return groupBusy;
      },
      get memberBusy() {
        return memberBusy;
      },
      get error() {
        return groupError;
      },
      remove: {
        get open() {
          return removeGroupTarget !== null;
        },
        get target() {
          return removeGroupTarget;
        },
        get busy() {
          return removeGroupBusy;
        },
        get error() {
          return removeGroupError;
        },
        ask: askRemoveGroup,
        confirm: confirmRemoveGroup,
        cancel: closeGroupRemove,
      },
      create: openGroupCreate,
      show: openGroupDialog,
      save: saveGroup,
      addMember,
      removeMember,
      cancel: closeGroup,
    },
  };
}
