// The user console's set dialog, its create path and its delete confirm.
// The dialog owns the target, the save's latch and a stopped save's report;
// the page keeps the read that seeds it, since that read also feeds the
// page's other sections.
//
// Opening the dialog has one path, and it carries a request token: the token
// bumps first, then the load runs and compares before it seeds, and again
// after a read the dialog needed before it could open. Only the newest click's
// answer may seed the dialog: a slow loadSet answering after the user moved on
// (to another set, or to the create form) would swap the edit target under the
// open dialog, and a save would then PATCH one set's members onto another's
// registry entry. The token outlives the dialog on purpose: resetting it on
// close would let a read that answers after the close pass the compare and
// seed the freshly opened dialog with the set the user left.
//
// A member list projects onto rows the store has read. With the rows unread
// the detail comes out empty and the first save would clear the set, so the
// edit reads once and opens only over a detail it could build.
// A delete confirm gets its own latch by default; it shares the form's only
// where closing the form is part of the delete.

import type { UserProfileRow, UserSetRow } from "./client.ts";
import { createSetDraft } from "./createSet.ts";
import { setDetailFor, type UserSetDetail } from "./userSetDraft.ts";
import type { SetSaveDraft } from "./setDraft.ts";
import { saveSetDraft, saveStoppedBanner } from "./saveSet.ts";

type UserGatewayStore =
  (typeof import("./store.svelte.ts"))["userGatewayStore"];

/** The store calls the set dialogs make, plus the reason it reports. */
export type MineSetDialogStore = Pick<
  UserGatewayStore,
  | "loadSet"
  | "addSet"
  | "updateMember"
  | "patchDescription"
  | "reorderSet"
  | "refreshAll"
  | "removeSet"
  | "profiles"
  | "error"
>;

export interface MineSetDialogOptions {
  /** The store slice these dialogs write through. */
  readonly store: MineSetDialogStore;
}

/** The set dialog's delete confirm. */
export interface MineSetRemoveGroup {
  readonly open: boolean;
  readonly target: UserSetRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  ask(set: UserSetRow): void;
  confirm(): Promise<void>;
  cancel(): void;
}

/** The set dialog, its create form and its delete confirm. */
export interface MineSetDialogs {
  readonly open: boolean;
  readonly set: UserSetDetail | null;
  readonly collection: string;
  readonly profiles: readonly UserProfileRow[] | null;
  readonly busy: boolean;
  readonly error: string | null;
  readonly landed: readonly string[];
  readonly remove: MineSetRemoveGroup;
  edit(set: UserSetRow): Promise<void>;
  create(collection: string): void;
  save(result: SetSaveDraft): Promise<void>;
  cancel(): void;
}

export function createMineSetDialogs(
  options: MineSetDialogOptions,
): MineSetDialogs {
  const store = options.store;

  /** The collection's rows: the dialog's member pool and the rows a member
   * id resolves to. Null while the store has not read the rows: rows never
   * read are not rows known to be absent, and a member list projected onto
   * unknown rows would read every member as deleted. */
  function rowsIn(collection: string): readonly UserProfileRow[] | null {
    const all = store.profiles;
    if (all === null) return null;
    return all.filter((row) => row.collection_name === collection);
  }

  let editSet = $state<UserSetDetail | null>(null);
  let dialogOpen = $state(false);
  // Pure bookkeeping: no effect should track this counter.
  let editRequest = 0;
  // The create dialog's pre-bound collection (the group it was opened from).
  let createCollection = $state("");
  let dialogBusy = $state(false);
  let dialogError = $state<string | null>(null);
  let dialogLanded = $state<readonly string[]>([]);
  let removeTarget = $state<UserSetRow | null>(null);
  let removeBusy = $state(false);
  let removeError = $state<string | null>(null);

  /** The collection the dialog works in: the row under edit, or the one a
   * create was opened from. */
  function activeCollection(): string {
    return editSet === null ? createCollection : editSet.collection_name;
  }

  /** The one way in: the token bumps first, and a null set is the create
   * form, which reads no row and takes the collection it was opened from. */
  async function open(
    target: UserSetRow | null,
    // Only the create branch reads it: an edit carries no collection.
    collection = "",
  ): Promise<void> {
    const request = ++editRequest;
    if (target === null) {
      editSet = null;
      createCollection = collection;
      dialogOpen = true;
      return;
    }
    const row = await store.loadSet(target.collection_name, target.name);
    if (row === undefined || request !== editRequest) return;
    if (store.profiles === null) {
      // Read once, then open only when the rows are known; the page alert
      // carries the read's reason.
      await store.refreshAll();
      if (request !== editRequest) return;
    }
    const detail = setDetailFor(row, rowsIn(row.collection_name));
    if (detail === null) return;
    editSet = detail;
    dialogOpen = true;
  }

  function edit(set: UserSetRow): Promise<void> {
    return open(set);
  }

  function create(collection: string): void {
    void open(null, collection);
  }

  // One save spans several requests, so the read side refreshes once, in
  // the sequence (its settle port), never per write: the store's
  // updateMember, patchDescription and reorderSet only write.
  async function save(result: SetSaveDraft): Promise<void> {
    const editing = editSet;
    dialogBusy = true;
    dialogError = null;
    try {
      let target: UserSetDetail | null = editing;
      let draft = result;
      if (target === null) {
        // The create's pre-flight owns the picks: an empty draft needs no
        // rows, and a draft whose rows stay unread after one read is
        // refused there, before the set exists. The page alert carries the
        // refusal's reason, as it carries a failed write's.
        const start = await createSetDraft(result, {
          rows: () => rowsIn(createCollection),
          refresh: () => store.refreshAll(),
          addSet: (name, description) =>
            store.addSet(createCollection, { name, description }),
        });
        if (start === undefined) {
          dialogError = store.error;
          return;
        }
        target = start.target;
        draft = start.draft;
      }
      const anchor = target;
      const outcome = await saveSetDraft(anchor, draft, {
        putMember: async (profileId, body) => {
          const ok = await store.updateMember(
            anchor.collection_name,
            profileId,
            body,
          );
          return { ok, reason: ok ? null : store.error };
        },
        patchDescription: async (description) => {
          const ok = await store.patchDescription(
            anchor.collection_name,
            anchor.name,
            description,
          );
          return { ok, reason: ok ? null : store.error };
        },
        reorder: async (members) => {
          const ok = await store.reorderSet(
            anchor.collection_name,
            anchor.name,
            members,
          );
          return { ok, reason: ok ? null : store.error };
        },
        // The rows a PUT just changed are not in the page's cache yet: the
        // re-read below projects them, so the refresh has to land first.
        settle: () => store.refreshAll(),
        reread: async () => {
          const row = await store.loadSet(anchor.collection_name, anchor.name);
          if (row === undefined) return undefined;
          // Rows still unread answer undefined: the draft keeps what it has
          // rather than reseeding from an empty member list.
          return setDetailFor(row, rowsIn(anchor.collection_name)) ?? undefined;
        },
      });
      if (outcome.phase !== "done") {
        // The landed list lands before the re-read row: the dialog's
        // effect short-circuits on set === seeded, so these two
        // assignments are not interchangeable.
        dialogLanded = outcome.landed;
        dialogError = saveStoppedBanner(outcome);
        if (outcome.row) editSet = outcome.row;
        return;
      }
      dialogOpen = false;
      editSet = null;
      dialogLanded = [];
    } finally {
      dialogBusy = false;
    }
  }

  function cancel(): void {
    if (dialogBusy) return;
    dialogOpen = false;
    editSet = null;
    dialogError = null;
  }

  function askRemove(set: UserSetRow): void {
    removeTarget = set;
    removeError = null;
  }

  async function confirmRemove(): Promise<void> {
    const row = removeTarget;
    if (row === null) return;
    removeBusy = true;
    removeError = null;
    await store.removeSet(row.collection_name, row.name);
    removeBusy = false;
    if (store.error !== null) {
      removeError = store.error;
    } else {
      removeTarget = null;
    }
  }

  return {
    get open() {
      return dialogOpen;
    },
    get set() {
      return editSet;
    },
    get collection() {
      return activeCollection();
    },
    get profiles() {
      return rowsIn(activeCollection());
    },
    get busy() {
      return dialogBusy;
    },
    get error() {
      return dialogError;
    },
    get landed() {
      return dialogLanded;
    },
    edit,
    create,
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
        return removeBusy;
      },
      get error() {
        return removeError;
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
