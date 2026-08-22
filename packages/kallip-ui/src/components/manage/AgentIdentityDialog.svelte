<script lang="ts">
  // Identity edit dialog for the agent detail page (TierDialog pattern:
  // prop-driven, never touches a store). Replaces the page's old inline
  // transparent-button editing, whose only affordance was a hover opacity
  // change; a visible menu item routes here instead. Fields seed from the
  // agent's current metadata on each open transition (plain latch).
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    common_cancel,
    common_save,
    manage_agent_edit_dialog_title,
    manage_agent_edit_dialog_desc,
    manage_agent_role,
    manage_agent_description,
  } from "../../paraglide/messages.js";

  let {
    open,
    role,
    description,
    busy = false,
    onSave,
    onCancel,
  }: {
    open: boolean;
    role: string;
    description: string;
    /** Mirrors the page's in-flight gate so double submits stay blocked. */
    busy?: boolean;
    onSave: (role: string, description: string) => void;
    onCancel: () => void;
  } = $props();

  let roleDraft = $state("");
  let descDraft = $state("");
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      roleDraft = role;
      descDraft = description;
    }
    lastOpen = open;
  });

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open) onCancel();
  }

  function submit(): void {
    if (busy) return;
    onSave(roleDraft, descDraft);
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
          {manage_agent_edit_dialog_title()}
        </Dialog.Title>
        <Dialog.Description class="sr-only">
          {manage_agent_edit_dialog_desc()}
        </Dialog.Description>

        <form
          class="flex flex-col gap-4"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">{manage_agent_role()}</span>
            <input class="input text-sm" bind:value={roleDraft} />
          </label>
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">{manage_agent_description()}</span
            >
            <input class="input text-sm" bind:value={descDraft} />
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
              disabled={busy}
            >
              {common_save()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
