<script lang="ts">
  // The owner-management surface for a tagma's joined rooms: lists every room
  // the tagma is a member of and lets the owner pull it out of any of them
  // (the cross-room owner-pulls-agent path -- the owner need not be a member of
  // the room). Lazily fetched: the list is loaded ONLY when the dialog opens, so
  // the tagma dashboard overview never pays a request per card.
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import type { TagmaRoomView } from "@kallipai/kallip-lesche-client";
  import ConfirmDialog from "../ConfirmDialog.svelte";
  import { lescheClientOrFail } from "../../lib/session/agora.svelte";
  import { roomsStore } from "../../lib/session/rooms.svelte";

  let {
    open,
    tagmaId,
    tagmaLabel,
    onCancel,
  }: {
    open: boolean;
    tagmaId: string;
    tagmaLabel: string | null | undefined;
    onCancel: () => void;
  } = $props();

  let rooms = $state<TagmaRoomView[]>([]);
  let loading = $state(false);
  let loadError = $state<string | null>(null);

  // Per-room removal confirmation.
  let confirmTarget = $state<TagmaRoomView | null>(null);
  let removeBusy = $state(false);
  let removeError = $state<string | null>(null);

  function roomLabel(r: TagmaRoomView): string {
    return r.name?.trim() || `room ${r.room_id.slice(0, 8)}`;
  }

  async function fetchRooms(): Promise<void> {
    loading = true;
    loadError = null;
    try {
      rooms = await lescheClientOrFail().listMyTagmaRooms(tagmaId);
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  // Fetch only when the dialog opens (and when the target tagma changes while
  // open). The dashboard never opens this dialog, so the overview pays nothing.
  $effect(() => {
    if (!open) return;
    void fetchRooms();
  });

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open) {
      // Drop the previous tagma's list + any pending confirm so a reopen does
      // not flash stale state before the fetch resolves.
      rooms = [];
      loadError = null;
      confirmTarget = null;
      removeError = null;
      onCancel();
    }
  }

  async function confirmRemove(): Promise<void> {
    const target = confirmTarget;
    if (!target || removeBusy) return;
    removeBusy = true;
    removeError = null;
    try {
      await roomsStore.removeTagmaFromRoom(target.room_id, tagmaId);
      await fetchRooms();
      confirmTarget = null;
    } catch (e) {
      removeError = e instanceof Error ? e.message : String(e);
    } finally {
      removeBusy = false;
    }
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-sm p-6 flex flex-col gap-4"
      >
        <Dialog.Title class="text-lg font-semibold">
          {tagmaLabel ?? "Tagma"} rooms
        </Dialog.Title>
        <Dialog.Description class="text-sm opacity-80">
          Rooms this tagma has joined. You can remove it from any of them, even
          a room you are not a member of.
        </Dialog.Description>

        {#if loadError}
          <p class="text-error-500 text-xs">
            Could not load rooms: {loadError}
          </p>
        {:else if loading}
          <p class="text-sm opacity-60">Loading…</p>
        {:else if rooms.length === 0}
          <p class="text-sm opacity-60">This tagma has not joined any rooms.</p>
        {:else}
          <ul class="flex flex-col gap-1 max-h-[50vh] overflow-auto">
            {#each rooms as r (r.room_id)}
              <li class="flex items-center justify-between gap-2 text-sm">
                <span class="truncate">{roomLabel(r)}</span>
                <button
                  type="button"
                  class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500 shrink-0"
                  onclick={() => {
                    removeError = null;
                    confirmTarget = r;
                  }}
                >
                  Remove
                </button>
              </li>
            {/each}
          </ul>
        {/if}

        <div class="flex justify-end">
          <button
            type="button"
            class="btn preset-outlined-surface-500"
            onclick={onCancel}
          >
            Close
          </button>
        </div>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>

<ConfirmDialog
  open={confirmTarget !== null}
  title="Remove tagma from room?"
  description={confirmTarget
    ? `Remove ${tagmaLabel ?? "this tagma"} from "${roomLabel(confirmTarget)}"? It will stop receiving messages there.`
    : ""}
  confirmLabel={removeBusy ? "Removing…" : "Remove"}
  busy={removeBusy}
  tone="danger"
  error={removeError ? `Remove failed: ${removeError}` : null}
  onConfirm={confirmRemove}
  onCancel={() => {
    confirmTarget = null;
    removeError = null;
  }}
/>
