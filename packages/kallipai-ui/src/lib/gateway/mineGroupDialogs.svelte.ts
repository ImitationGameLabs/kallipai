// The user console's group dialog and its delete confirm, sharing one latch.
// The edit opens over a fresh read of the group: a group's members are the
// audience the publish dialog binds collections to, so the form edits what
// the registry holds rather than what a list row carried.
//
// Opening the dialog has one path, and it carries a request token: the click
// bumps it, the read compares against it, and only the newest answer may seed
// the dialog. The create form is that same path with a null target, so a read
// still in flight cannot land its group in it.
// A delete confirm gets its own latch by default; it shares the form's only
// where closing the form is part of the delete.

import type { UserGroupRow } from "./client.ts";

type UserGatewayStore =
  (typeof import("./store.svelte.ts"))["userGatewayStore"];

/** The store calls the group dialogs make, plus the reason it reports. */
export type MineGroupDialogStore = Pick<
  UserGatewayStore,
  "loadGroup" | "addGroup" | "saveGroup" | "removeGroup" | "error"
>;

export interface MineGroupDialogOptions {
  /** The store slice these dialogs write through. */
  readonly store: MineGroupDialogStore;
}

/** The group form; a null row is the create form. */
export interface MineGroupEditGroup {
  readonly open: boolean;
  readonly row: UserGroupRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  show(row: UserGroupRow | null): Promise<void>;
  save(result: { name: string; members: string[] }): Promise<void>;
  cancel(): void;
}

/** The group delete confirm. Its latch is the form's: a refused delete has
 * to report where the user is looking. */
export interface MineGroupRemoveGroup {
  readonly open: boolean;
  readonly target: UserGroupRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  ask(row: UserGroupRow): void;
  confirm(): Promise<void>;
  cancel(): void;
}

/** The group form and its delete confirm. */
export interface MineGroupDialogs {
  readonly edit: MineGroupEditGroup;
  readonly remove: MineGroupRemoveGroup;
}

export function createMineGroupDialogs(
  options: MineGroupDialogOptions,
): MineGroupDialogs {
  const store = options.store;

  let editGroup = $state<UserGroupRow | null>(null);
  let dialogOpen = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let removeTarget = $state<UserGroupRow | null>(null);
  // Pure bookkeeping: no effect should track this counter.
  let editRequest = 0;

  async function show(row: UserGroupRow | null): Promise<void> {
    const request = ++editRequest;
    if (row === null) {
      // The create form reads no row; the bump above is what keeps a read
      // still in flight from seeding it.
      busy = false;
      error = null;
      editGroup = null;
      dialogOpen = true;
      return;
    }
    busy = false;
    error = null;
    const full = await store.loadGroup(row.group_id);
    if (full !== undefined && request === editRequest) {
      editGroup = full;
      dialogOpen = true;
    }
  }

  async function save(result: {
    name: string;
    members: string[];
  }): Promise<void> {
    busy = true;
    error = null;
    const ok =
      editGroup === null
        ? await store.addGroup({ name: result.name, members: result.members })
        : await store.saveGroup(editGroup.group_id, {
            name: result.name,
            members: result.members,
          });
    busy = false;
    if (!ok) {
      error = store.error;
      return;
    }
    dialogOpen = false;
    editGroup = null;
  }

  function cancel(): void {
    dialogOpen = false;
    editGroup = null;
    error = null;
  }

  function askRemove(row: UserGroupRow): void {
    removeTarget = row;
    error = null;
  }

  async function confirmRemove(): Promise<void> {
    const id = removeTarget?.group_id;
    if (!id) return;
    busy = true;
    error = null;
    await store.removeGroup(id);
    busy = false;
    if (store.error !== null) {
      error = store.error;
    } else {
      removeTarget = null;
    }
  }

  return {
    edit: {
      get open() {
        return dialogOpen;
      },
      get row() {
        return editGroup;
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
    },
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
