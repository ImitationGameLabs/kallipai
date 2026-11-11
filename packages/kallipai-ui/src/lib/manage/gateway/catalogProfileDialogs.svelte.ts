// The catalog console's profile dialogs: the unassigned profile form with its
// remove confirm, and the placement dialog the list view's drop fallback opens.
//
// A delete confirm gets its own latch by default; it shares the form's only
// where closing the form is part of the delete.
//
// The placement path and the drop path share one busy latch: both PUT the same
// membership row, and a second drop must not race the first. The reload after
// a drop runs only on a clean store verdict — the reload's own read nulls the
// error first, so an unconditional reload would wipe the failure banner before
// it paints.

import type { AdminProfileRow } from "./client.ts";

type GatewayAdminStore = (typeof import("./store.svelte.ts"))["gatewayStore"];

/** The store calls the profile dialogs make, plus the reason it reports. */
export type CatalogProfileDialogStore = Pick<
  GatewayAdminStore,
  | "addUnassigned"
  | "saveUnassigned"
  | "removeUnassigned"
  | "addUnassignedToSet"
  | "error"
>;

/** The one call a drop handler makes on its event. This module runs under
 * deno test, which has no DOM lib, so the handler takes the shape it uses
 * rather than DragEvent; a real DragEvent is assignable to it. */
export interface DropEvent {
  preventDefault(): void;
}

export interface CatalogProfileDialogOptions {
  /** The store slice these dialogs write through. */
  readonly store: CatalogProfileDialogStore;
  /** Re-read the open collection's unassigned rows. */
  readonly reloadUnassigned: () => Promise<void>;
}

/** A profile row as the unassigned form hands it back. */
export interface UnassignedDraft {
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

/** The unassigned delete confirm, on the form's latch. */
export interface CatalogUnassignedRemoveGroup {
  readonly open: boolean;
  readonly target: AdminProfileRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  ask(row: AdminProfileRow): void;
  confirm(): Promise<void>;
  cancel(): void;
}

/** The unassigned profile form and its delete confirm. */
export interface CatalogUnassignedGroup {
  readonly open: boolean;
  readonly row: AdminProfileRow | null;
  readonly busy: boolean;
  readonly error: string | null;
  readonly remove: CatalogUnassignedRemoveGroup;
  create(): void;
  show(row: AdminProfileRow | null): void;
  save(result: UnassignedDraft & { collection: string }): Promise<void>;
  cancel(): void;
}

/** The drag lifecycle, the drop paths and the list view's fallback dialog. */
export interface CatalogPlacementGroup {
  readonly open: boolean;
  readonly row: AdminProfileRow | null;
  readonly busy: boolean;
  readonly dragRow: AdminProfileRow | null;
  readonly dragOverSet: string | null;
  startDrag(row: AdminProfileRow): void;
  dragOver(name: string): void;
  dragLeave(name: string): void;
  clearDrag(): void;
  dropToSet(name: string): Promise<void>;
  dropAnywhere(e: DropEvent): void;
  confirm(setName: string): Promise<void>;
  cancel(): void;
}

/** The unassigned profile dialogs and the placement ones. */
export interface CatalogProfileDialogs {
  readonly unassigned: CatalogUnassignedGroup;
  readonly placement: CatalogPlacementGroup;
}

export function createCatalogProfileDialogs(
  options: CatalogProfileDialogOptions,
): CatalogProfileDialogs {
  const store = options.store;
  const reloadUnassigned = options.reloadUnassigned;

  // -- unassigned profiles ------------------------------------------------

  // The unassigned form and its remove confirm share one latch pair: a
  // refused delete reports on the panel the user is looking at.
  let editUnassigned = $state<AdminProfileRow | null>(null);
  let unassignedDialogOpen = $state(false);
  let unassignedBusy = $state(false);
  let unassignedError = $state<string | null>(null);
  let removeUnassignedTarget = $state<AdminProfileRow | null>(null);

  function openUnassignedDialog(row: AdminProfileRow | null): void {
    editUnassigned = row;
    unassignedError = null;
    unassignedDialogOpen = true;
  }

  function openUnassignedCreate(): void {
    openUnassignedDialog(null);
  }

  async function saveUnassigned(
    result: UnassignedDraft & {
      collection: string;
    },
  ): Promise<void> {
    unassignedBusy = true;
    unassignedError = null;
    if (editUnassigned === null) {
      const ok = await store.addUnassigned(result.collection, {
        profile_id: result.profile_id,
        provider_id: result.provider_id,
        model: result.model,
        max_context_window: result.max_context_window,
        effort: result.effort,
        modalities: result.modalities,
      });
      unassignedBusy = false;
      if (!ok) {
        unassignedError = store.error;
        return;
      }
    } else {
      // The row decides the set this PUT carries; the dialog edits content.
      const ok = await store.saveUnassigned(editUnassigned, result);
      unassignedBusy = false;
      if (!ok) {
        unassignedError = store.error;
        return;
      }
    }
    await reloadUnassigned();
    unassignedDialogOpen = false;
    editUnassigned = null;
  }

  function askRemoveUnassigned(row: AdminProfileRow): void {
    removeUnassignedTarget = row;
    unassignedError = null;
  }

  async function confirmRemoveUnassigned(): Promise<void> {
    const row = removeUnassignedTarget;
    if (row === null) return;
    unassignedBusy = true;
    unassignedError = null;
    await store.removeUnassigned(row.collection_name, row.profile_id);
    unassignedBusy = false;
    if (store.error !== null) {
      unassignedError = store.error;
    } else {
      await reloadUnassigned();
      removeUnassignedTarget = null;
    }
  }

  function closeUnassigned(): void {
    unassignedDialogOpen = false;
    editUnassigned = null;
    unassignedError = null;
  }

  function closeUnassignedRemove(): void {
    removeUnassignedTarget = null;
    unassignedError = null;
  }

  // -- unassigned placement (drag, drop, the fallback dialog) -------------

  // The page owns the drag state (the sections report the raw lifecycle),
  // and a busy latch keeps a second drop from racing the in-flight member
  // update. The drop carries the whole row: the membership PUT sends the
  // row's own content with the destination set.
  let dragRow = $state<AdminProfileRow | null>(null);
  let dragOverSet = $state<string | null>(null);
  let dropBusy = $state(false);
  let placementOpen = $state(false);
  let placementRow = $state<AdminProfileRow | null>(null);

  function startDrag(row: AdminProfileRow): void {
    dragRow = row;
  }

  function dragOver(name: string): void {
    dragOverSet = name;
  }

  function dragLeave(name: string): void {
    dragOverSet = name === dragOverSet ? null : dragOverSet;
  }

  function clearDrag(): void {
    dragRow = null;
    dragOverSet = null;
  }

  async function onDropToSet(setName: string): Promise<void> {
    const row = dragRow;
    clearDrag();
    if (row === null || dropBusy) return;
    dropBusy = true;
    try {
      await store.addUnassignedToSet(row, setName);
      // Re-read only on success: the reload's own read nulls the error
      // first (store.run), so an unconditional reload would wipe the
      // failure banner before it paints.
      if (store.error === null) await reloadUnassigned();
    } finally {
      dropBusy = false;
    }
  }

  // The list view's drop fallback: with no detail open there is no
  // set card to land on, so the drop opens the placement dialog
  // (the dragged row rides along; the busy latch is shared).
  function onDropAnywhere(e: DropEvent): void {
    e.preventDefault();
    const row = dragRow;
    clearDrag();
    if (row === null) return;
    placementRow = row;
    placementOpen = true;
  }

  async function confirmPlacement(setName: string): Promise<void> {
    const row = placementRow;
    if (row === null || dropBusy) return;
    dropBusy = true;
    try {
      await store.addUnassignedToSet(row, setName);
      if (store.error === null) {
        placementOpen = false;
        placementRow = null;
      }
    } finally {
      dropBusy = false;
    }
  }

  function closePlacement(): void {
    placementOpen = false;
    placementRow = null;
  }

  return {
    unassigned: {
      get open() {
        return unassignedDialogOpen;
      },
      get row() {
        return editUnassigned;
      },
      get busy() {
        return unassignedBusy;
      },
      get error() {
        return unassignedError;
      },
      remove: {
        get open() {
          return removeUnassignedTarget !== null;
        },
        get target() {
          return removeUnassignedTarget;
        },
        // The form's pair, because the refused delete reports on the form.
        get busy() {
          return unassignedBusy;
        },
        get error() {
          return unassignedError;
        },
        ask: askRemoveUnassigned,
        confirm: confirmRemoveUnassigned,
        cancel: closeUnassignedRemove,
      },
      create: openUnassignedCreate,
      show: openUnassignedDialog,
      save: saveUnassigned,
      cancel: closeUnassigned,
    },
    placement: {
      get open() {
        return placementOpen;
      },
      get row() {
        return placementRow;
      },
      get busy() {
        return dropBusy;
      },
      get dragRow() {
        return dragRow;
      },
      get dragOverSet() {
        return dragOverSet;
      },
      startDrag,
      dragOver,
      dragLeave,
      clearDrag,
      dropToSet: onDropToSet,
      dropAnywhere: onDropAnywhere,
      confirm: confirmPlacement,
      cancel: closePlacement,
    },
  };
}
