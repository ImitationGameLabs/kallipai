<script lang="ts" module>
  // Catalog set edit dialog: the description field plus the member
  // order (the failover sequence) as a draggable card list. This is
  // the face's one composite edit, so the dialog holds a local draft
  // (dialog-scoped, not a page-level draft session) and hands the
  // whole order back; the page lands it as one
  // PATCH. Sets are never renamed here: the name is the registry key.
  import type { AdminProfileRow } from "../../../lib/manage/gateway/client.ts";

  /** What save reports: the edited description plus the member order. */
  export interface GatewaySetDialogResult {
    readonly description: string;
    readonly members: string[];
  }
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    common_cancel,
    common_save,
    manage_profiles_set_dialog_edit_title,
    manage_profiles_set_dialog_description_label,
  } from "../../../paraglide/messages.js";
  import GatewayMemberCard from "./GatewayMemberCard.svelte";
  import type { AdminSetRow } from "../../../lib/manage/gateway/client.ts";

  let {
    open,
    set,
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The set under edit (null = nothing open; the dialog idles). */
    set: AdminSetRow | null;
    onSave: (result: GatewaySetDialogResult) => void;
    onCancel: () => void;
  } = $props();

  // The local draft: description plus the full member rows (rows, not
  // bare ids, so the cards keep rendering during the reorder). Seeded
  // on each open transition (plain latch, no self-trigger).
  let descriptionDraft = $state("");
  let members = $state<AdminProfileRow[]>([]);
  let dragFrom = $state<number | null>(null);
  let dragOver = $state<number | null>(null);
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      descriptionDraft = set?.description ?? "";
      members = set ? [...set.profiles] : [];
      dragFrom = null;
      dragOver = null;
    }
    lastOpen = open;
  });

  // An empty member list cannot be saved: the order is the set's
  // content, and an empty PATCH would blank the registry row.
  const canSubmit = $derived(members.length > 0);

  function clearDrag(): void {
    dragFrom = null;
    dragOver = null;
  }

  function reorder(fromIdx: number, toIdx: number): void {
    if (fromIdx === toIdx) return;
    const next = [...members];
    const [moved] = next.splice(fromIdx, 1);
    if (!moved) return;
    next.splice(toIdx, 0, moved);
    members = next;
  }

  // Removal joins the draft: the row leaves the list here, and the
  // membership change (with its served-face flip) lands on save.
  function removeAt(idx: number): void {
    const next = [...members];
    next.splice(idx, 1);
    members = next;
  }

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open) onCancel();
  }

  function submit(): void {
    if (!canSubmit || !set) return;
    onSave({
      description: descriptionDraft,
      members: members.map((m) => m.profile_id),
    });
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-xl p-6 flex flex-col gap-4 max-h-[85vh] overflow-y-auto"
      >
        <Dialog.Title class="text-lg font-semibold">
          {manage_profiles_set_dialog_edit_title()}
          <span class="font-mono opacity-80">{set?.name}</span>
        </Dialog.Title>

        <form
          class="flex flex-col gap-4"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {manage_profiles_set_dialog_description_label()}
            </span>
            <input class="input text-sm" bind:value={descriptionDraft} />
          </label>

          <div role="list" class="flex flex-col gap-2">
            {#each members as member, idx (member.profile_id)}
              <GatewayMemberCard
                {member}
                position={idx + 1}
                dragOver={dragOver === idx}
                onDragStart={() => (dragFrom = idx)}
                onDragEnd={clearDrag}
                onDragOver={() => (dragOver = idx)}
                onDrop={() => {
                  if (dragFrom !== null) reorder(dragFrom, idx);
                  clearDrag();
                }}
                onRemove={() => removeAt(idx)}
              />
            {/each}
          </div>

          <div class="flex gap-2">
            <button
              type="button"
              class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"
              onclick={onCancel}
            >
              {common_cancel()}
            </button>
            <button
              type="submit"
              class="btn flex-1 preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110"
              disabled={!canSubmit}
            >
              {common_save()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
