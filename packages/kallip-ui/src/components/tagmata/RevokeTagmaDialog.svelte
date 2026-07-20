<script lang="ts">
  // A confirm modal for revoking an enrolled tagma. Revocation is one-click
  // irreversible AND functionally immediate (the agora cuts the herald off on
  // its next request), so it gets a second-chance confirmation the pending-code
  // revoke does not. Plain controlled overlay (the trigger is a Menu item, not a
  // button, so a programmatic `open` is simpler than wiring Dialog.Trigger).
  // Dismissable via the Cancel button, the backdrop button, or Escape.
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
</script>

<svelte:window
  onkeydown={(e) => e.key === "Escape" && open && !busy && onCancel()}
/>

{#if open}
  <div class="fixed inset-0 z-50 grid place-items-center p-4">
    <!-- Backdrop: a real button so click-to-dismiss is keyboard-accessible.
         Sits behind the panel (DOM order + the panel's `relative`). -->
    <button
      type="button"
      tabindex="-1"
      class="absolute inset-0 bg-surface-950-50/60 cursor-default"
      aria-label="Cancel revoke"
      disabled={busy}
      onclick={onCancel}
    ></button>
    <div
      class="card preset-tonal-surface w-full max-w-sm p-6 flex flex-col gap-4 relative"
      role="dialog"
      aria-modal="true"
      aria-labelledby="revoke-tagma-title"
    >
      <div>
        <h2 id="revoke-tagma-title" class="text-lg font-semibold">
          Revoke tagma?
        </h2>
        <p class="text-sm opacity-80 mt-1">
          {tagmaLabel ? `"${tagmaLabel}"` : "This tagma"} will lose access immediately.
          The device is disconnected on its next attempt to reach the server, and
          the tagma disappears from this list.
        </p>
      </div>
      {#if error}
        <p class="text-error-500 text-xs">Revoke failed: {error}</p>
      {/if}
      <div class="flex justify-end gap-2">
        <button
          type="button"
          class="btn preset-tonal-surface"
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
    </div>
  </div>
{/if}
