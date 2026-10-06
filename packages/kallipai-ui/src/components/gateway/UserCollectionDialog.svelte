<script lang="ts" module>
  // Collection edit dialog: the description plus the set membership
  // (unordered; each member set keeps its own failover order). The
  // name is the registry key: no rename.
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    common_cancel,
    common_save,
    user_gateway_collection_edit_title,
    user_gateway_collection_sets,
    user_gateway_set_description,
  } from "../../paraglide/messages.js";

  let {
    open,
    collection,
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The collection under edit (null = the dialog idles). */
    collection: { name: string; description: string; sets: string[] } | null;
    onSave: (result: { description: string; sets: string[] }) => void;
    onCancel: () => void;
  } = $props();

  let descriptionDraft = $state("");
  let setsDraft = $state("");
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      descriptionDraft = collection?.description ?? "";
      setsDraft = collection ? collection.sets.join("\n") : "";
    }
    lastOpen = open;
  });

  function parseSets(raw: string): string[] {
    return raw
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line !== "");
  }

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open) onCancel();
  }

  function submit(): void {
    if (!collection) return;
    onSave({ description: descriptionDraft, sets: parseSets(setsDraft) });
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-md p-6 flex flex-col gap-4"
      >
        <Dialog.Title class="text-lg font-semibold">
          {user_gateway_collection_edit_title()}
          <span class="font-mono opacity-80">{collection?.name}</span>
        </Dialog.Title>

        <form
          class="flex flex-col gap-3"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {user_gateway_set_description()}
            </span>
            <input class="input text-sm" bind:value={descriptionDraft} />
          </label>
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {user_gateway_collection_sets()}
            </span>
            <textarea
              class="textarea text-sm font-mono"
              rows="5"
              bind:value={setsDraft}></textarea>
          </label>

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
            >
              {common_save()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
