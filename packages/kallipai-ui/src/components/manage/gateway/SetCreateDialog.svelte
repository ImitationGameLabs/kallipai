<script lang="ts" module>
  // Catalog set creation dialog: the name (the registry key), the
  // optional description. The dialog opens from inside a collection
  // detail (the collection is pre-bound, not a pick here) and holds
  // a local draft seeded on each open transition; the page lands
  // the fields as one POST through the store's add wrap. Duplicate
  // names disable the save (the name is the registry key, so a
  // collision is refused).
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import ReqMark from "../../ReqMark.svelte";
  import {
    common_cancel,
    common_save,
    manage_gateway_set_add,
    manage_profiles_set_dialog_description_label,
    manage_profiles_set_dialog_name_invalid,
    manage_profiles_set_dialog_name_label,
    manage_profiles_set_dialog_name_taken,
  } from "../../../paraglide/messages.js";

  let {
    open,
    names = [],
    busy = false,
    error = null,
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The existing set names: a collision disables the save. */
    names: readonly string[];
    busy?: boolean;
    error?: string | null;
    onSave: (result: { name: string; description: string }) => void;
    onCancel: () => void;
  } = $props();

  // The local draft, reset on each open transition (plain latch).
  let nameDraft = $state("");
  let descriptionDraft = $state("");
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      nameDraft = "";
      descriptionDraft = "";
    }
    lastOpen = open;
  });

  // Narrower than the server's rule (the gateway refuses empty
  // names, '/' and ':'); a duplicate name is refused server-side.
  const nameError = $derived(
    !/^[A-Za-z0-9_-]+$/.test(nameDraft)
      ? manage_profiles_set_dialog_name_invalid()
      : names.includes(nameDraft.trim())
        ? manage_profiles_set_dialog_name_taken()
        : null,
  );
  const canSubmit = $derived(nameError === null);

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }

  function submit(): void {
    if (!canSubmit || busy) return;
    onSave({
      name: nameDraft.trim(),
      description: descriptionDraft.trim(),
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
          {manage_gateway_set_add()}
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
              {manage_profiles_set_dialog_name_label()}
              <ReqMark />
            </span>
            <input
              class="input text-sm font-mono"
              bind:value={nameDraft}
              disabled={busy}
              aria-required="true"
            />
          </label>
          {#if nameError}
            <p class="text-xs text-error-500 dark:text-error-400">
              {nameError}
            </p>
          {/if}

          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {manage_profiles_set_dialog_description_label()}
            </span>
            <input
              class="input text-sm"
              bind:value={descriptionDraft}
              disabled={busy}
            />
          </label>

          {#if error}
            <p class="text-sm text-error-500" role="alert">{error}</p>
          {/if}

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
              disabled={!canSubmit || busy}
            >
              {common_save()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
