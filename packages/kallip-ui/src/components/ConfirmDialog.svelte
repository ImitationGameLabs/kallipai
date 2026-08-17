<script lang="ts">
  // A generic confirmation modal: the shared shell for any one-click,
  // irreversible action that deserves a second chance (leave room, remove
  // member, pull a tagma from a room). Built on the skeleton-svelte `Dialog`
  // (controlled `open`); dismiss routes through `onCancel` and is suppressed
  // while `busy` so a transient failure is surfaced rather than dropped. `tone`
  // picks the confirm button's preset (`danger` for destructive actions,
  // `primary` otherwise).
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import { common_cancel } from "../paraglide/messages.js";

  let {
    open,
    title,
    description,
    confirmLabel,
    busy = false,
    tone = "primary",
    error = null,
    onConfirm,
    onCancel,
  }: {
    open: boolean;
    title: string;
    description: string;
    confirmLabel: string;
    busy?: boolean;
    tone?: "primary" | "danger";
    error?: string | null;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }

  const confirmClass = $derived(
    tone === "danger"
      ? "btn flex-1 preset-filled-error-500 text-on-error-500 transition hover:brightness-110"
      : "btn flex-1 preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110",
  );
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-sm p-6 flex flex-col gap-4"
      >
        <Dialog.Title class="text-lg font-semibold">{title}</Dialog.Title>
        <Dialog.Description class="text-sm opacity-80">
          {description}
        </Dialog.Description>
        {#if error}
          <p class="text-error-500 dark:text-error-400 text-xs">{error}</p>
        {/if}
        <div class="flex gap-2">
          <button
            type="button"
            class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"
            disabled={busy}
            onclick={onCancel}
          >
            {common_cancel()}
          </button>
          <button
            type="button"
            class={confirmClass}
            disabled={busy}
            onclick={onConfirm}
          >
            {busy ? "…" : confirmLabel}
          </button>
        </div>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
