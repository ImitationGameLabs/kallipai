<script lang="ts">
  // The shell of a create/edit dialog: the modal frame, the title, the
  // error line and the cancel/save footer. The rules every caller shares
  // live here. A backdrop click never closes a dialog whose save is
  // running, so a transient failure stays visible instead of being
  // dropped; the cancel button stays live, because a deliberate close is
  // not an accidental one and a save whose latch never clears must leave
  // a way out. The error line sits with the fields and the footer rather
  // than above them: it reports the button the user just pressed, and a
  // body scrolling under the height cap would otherwise carry the message
  // out of sight. The fields, the drafts, the validation and the payload
  // stay in the caller, which passes them as `children`; nothing about
  // what is being edited reaches this file. Delete confirmations use
  // `ConfirmDialog` instead: they ask a question, they do not edit a draft.
  import type { Snippet } from "svelte";
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import { common_cancel, common_save } from "../paraglide/messages.js";

  let {
    open,
    title,
    titleExtra = undefined,
    size = "md",
    tall = false,
    busy,
    error,
    canSubmit,
    onSubmit,
    onCancel,
    onOpen = undefined,
    children,
  }: {
    open: boolean;
    title: string;
    /** A tag, a key or an id shown after the title, in mono. */
    titleExtra?: Snippet;
    /** The frame width: `md` unless the caller passes `xl`. */
    size?: "md" | "xl";
    /** Whether the body scrolls inside an 85vh cap. */
    tall?: boolean;
    busy: boolean;
    error: string | null;
    canSubmit: boolean;
    onSubmit: () => void;
    onCancel: () => void;
    /** Called on each open transition: the caller reseeds its drafts. */
    onOpen?: () => void;
    children: Snippet;
  } = $props();

  const WIDTH = { md: "max-w-md", xl: "max-w-xl" } as const;

  const contentClass = $derived(
    `card preset-tonal-surface w-full ${WIDTH[size]} p-6 flex flex-col gap-4` +
      (tall ? " max-h-[85vh] overflow-y-auto" : ""),
  );

  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) onOpen?.();
    lastOpen = open;
  });

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content class={contentClass}>
        <Dialog.Title class="text-lg font-semibold">
          {title}
          {#if titleExtra}{@render titleExtra()}{/if}
        </Dialog.Title>

        <form
          class="flex flex-col gap-4"
          onsubmit={(e) => {
            e.preventDefault();
            if (!canSubmit || busy) return;
            onSubmit();
          }}
        >
          {@render children()}

          {#if error}
            <p class="text-sm text-error-500 dark:text-error-400" role="alert">
              {error}
            </p>
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
