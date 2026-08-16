<script lang="ts">
  // A confirm modal for revoking an enrolled tagma. Revocation is one-click
  // irreversible AND functionally immediate (the agora cuts the tagma off on
  // its next request), so it gets a second-chance confirmation the pending-code
  // revoke does not. Built on the shared skeleton-svelte `Dialog` (controlled
  // `open`); Escape + backdrop dismiss come from the Zag machine defaults, both
  // routed through `onCancel`. Dismiss is suppressed while a revoke is in flight
  // so a transient failure is surfaced rather than dropped.
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";

  let {
    open,
    tagmaLabel,
    busy = false,
    error = null,
    onConfirm,
    onCancel,
  }: {
    open: boolean;
    tagmaLabel: string | null;
    busy?: boolean;
    error?: string | null;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  function onOpenChange(e: { open: boolean }): void {
    // Ignore Escape/backdrop close while the revoke is in flight.
    if (!e.open && !busy) onCancel();
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-sm p-6 flex flex-col gap-4"
      >
        <Dialog.Title class="text-lg font-semibold">Revoke tagma?</Dialog.Title>
        <Dialog.Description class="text-sm opacity-80">
          {tagmaLabel ? `"${tagmaLabel}"` : "This tagma"} will lose access immediately.
          The device is disconnected on its next attempt to reach the server, and
          the tagma disappears from this list.
        </Dialog.Description>
        {#if error}
          <p class="text-error-500 dark:text-error-400 text-xs">Revoke failed: {error}</p>
        {/if}
        <div class="flex justify-end gap-2">
          <button
            type="button"
            class="btn preset-outlined-surface-500"
            disabled={busy}
            onclick={onCancel}
          >
            Cancel
          </button>
          <button
            type="button"
            class="btn preset-filled-error-500 text-on-error-500"
            disabled={busy}
            onclick={onConfirm}
          >
            {busy ? "Revoking…" : "Revoke"}
          </button>
        </div>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
