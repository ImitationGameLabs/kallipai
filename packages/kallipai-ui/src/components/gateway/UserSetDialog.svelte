<script lang="ts" module>
  // Set dialog. Edit keeps the description plus the member order (the
  // order is the failover sequence, so the members edit as one
  // textarea, one id per line, top = first; the name is the registry
  // key, no rename). Create adds the name; the target collection is
  // pre-bound by the group the create was opened from.
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import ReqMark from "../ReqMark.svelte";
  import {
    common_cancel,
    common_save,
    user_gateway_set_add,
    user_gateway_set_description,
    user_gateway_set_dialog_collection_label,
    user_gateway_set_edit_title,
    user_gateway_set_members,
    user_gateway_set_name,
  } from "../../paraglide/messages.js";

  let {
    open,
    set,
    collection,
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The set under edit (null = create). */
    set: { name: string; description: string; members: string[] } | null;
    /** The collection a created set lands in (pre-bound). */
    collection: string;
    onSave: (result: {
      name: string;
      description: string;
      members: string[];
    }) => void;
    onCancel: () => void;
  } = $props();

  let nameDraft = $state("");
  let descriptionDraft = $state("");
  let membersDraft = $state("");
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      nameDraft = "";
      descriptionDraft = set?.description ?? "";
      membersDraft = set ? set.members.join("\n") : "";
    }
    lastOpen = open;
  });

  function parseMembers(raw: string): string[] {
    return raw
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line !== "");
  }

  // An empty member list cannot be saved: the order is the set's
  // content, and an empty PATCH would blank the row. Create mode also
  // needs a name (the collection came in with the open).
  const canSubmit = $derived(
    parseMembers(membersDraft).length > 0 &&
      (set !== null || nameDraft.trim() !== ""),
  );

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open) onCancel();
  }

  function submit(): void {
    if (!canSubmit) return;
    onSave({
      name: set ? set.name : nameDraft.trim(),
      description: descriptionDraft,
      members: parseMembers(membersDraft),
    });
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
          {set ? user_gateway_set_edit_title() : user_gateway_set_add()}
          {#if set}
            <span class="font-mono opacity-80">{set.name}</span>
          {/if}
        </Dialog.Title>

        <form
          class="flex flex-col gap-3"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          {#if set === null}
            <p class="text-xs opacity-60">
              {user_gateway_set_dialog_collection_label()}:
              <span class="font-mono">{collection}</span>
            </p>
            <label class="flex flex-col gap-1">
              <span class="text-xs font-medium">
                {user_gateway_set_name()}
                <ReqMark />
              </span>
              <input
                class="input text-sm font-mono"
                bind:value={nameDraft}
                aria-required="true"
              />
            </label>
          {/if}
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {user_gateway_set_description()}
            </span>
            <input class="input text-sm" bind:value={descriptionDraft} />
          </label>
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {user_gateway_set_members()}
              <ReqMark />
            </span>
            <textarea
              class="textarea text-sm font-mono"
              rows="5"
              aria-required="true"
              bind:value={membersDraft}></textarea>
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
