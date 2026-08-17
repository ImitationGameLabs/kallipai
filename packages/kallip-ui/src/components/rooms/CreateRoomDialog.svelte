<script lang="ts" module>
  // The new-room creation form, presented as a Skeleton dialog opened by the
  // dashboard's single "New Room" action. Replaces the old one-click dual
  // public/private buttons: a name is required, description is optional, and
  // visibility is a boolean toggle (private by default; click for public).
  //
  // Prop-driven: the dialog never touches a store. It calls `onCreate` with the
  // drafted fields; the owning page performs the create + post-create navigation
  // and closes the dialog on success (the `error` prop surfaces a failure).
  import type { Visibility } from "@kallipai/kallip-lesche-client";

  export interface CreateRoomOpts {
    name: string;
    description?: string;
    visibility?: Visibility;
  }
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    common_cancel,
    rooms_create_title,
    rooms_create_desc,
    rooms_name_label,
    rooms_name_placeholder,
    rooms_description_label,
    rooms_description_placeholder,
    rooms_visibility_aria,
    rooms_visibility_public,
    rooms_visibility_public_hint,
    rooms_visibility_private_hint,
    rooms_create_failed,
    rooms_creating,
    rooms_create_action,
  } from "../../paraglide/messages.js";

  let {
    open,
    busy = false,
    error = null,
    onCreate,
    onCancel,
  }: {
    open: boolean;
    busy?: boolean;
    error?: string | null;
    onCreate: (opts: CreateRoomOpts) => Promise<void> | void;
    onCancel: () => void;
  } = $props();

  // Drafts. Reset whenever the dialog opens so a prior draft (or error) does not
  // linger into the next "New Room". `lastOpen` is a plain (non-reactive) latch:
  // the effect tracks `open` only and fires once per open transition, with no
  // self-trigger cycle (it never reads reactive state it also writes).
  let name = $state("");
  let description = $state("");
  let visibility = $state<Visibility>("private");
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      name = "";
      description = "";
      visibility = "private";
    }
    lastOpen = open;
  });

  const canSubmit = $derived(name.trim().length > 0 && !busy);

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }

  function submit(): void {
    if (!canSubmit) return;
    void onCreate({
      name: name.trim(),
      description: description.trim(),
      visibility,
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
        <Dialog.Title class="text-lg font-semibold"
          >{rooms_create_title()}</Dialog.Title
        >
        <Dialog.Description class="sr-only">
          {rooms_create_desc()}
        </Dialog.Description>

        <form
          class="flex flex-col gap-4"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium"
              >{rooms_name_label()}
              <span class="text-error-500 dark:text-error-400">*</span></span
            >
            <input
              class="input text-sm"
              placeholder={rooms_name_placeholder()}
              maxlength={128}
              bind:value={name}
              disabled={busy}
              required
            />
          </label>

          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium">{rooms_description_label()}</span>
            <textarea
              class="input text-sm min-h-[4rem] resize-y"
              placeholder={rooms_description_placeholder()}
              maxlength={1024}
              bind:value={description}
              disabled={busy}></textarea>
          </label>

          <!-- A boolean visibility switch: off = private (default), on = public.
               Clicking toggles; the helper line explains the active mode. -->
          <div class="flex flex-col gap-1">
            <button
              type="button"
              role="switch"
              aria-checked={visibility === "public"}
              aria-label={rooms_visibility_aria()}
              class="flex items-center gap-3 text-left disabled:opacity-60"
              disabled={busy}
              onclick={() =>
                (visibility = visibility === "public" ? "private" : "public")}
            >
              <span
                class="relative inline-flex h-6 w-11 shrink-0 items-center rounded-full transition {visibility ===
                'public'
                  ? 'bg-primary-500'
                  : 'bg-surface-300-700'}"
              >
                <span
                  class="inline-block size-5 transform rounded-full bg-surface-50-950 shadow-sm transition {visibility ===
                  'public'
                    ? 'translate-x-5'
                    : 'translate-x-0.5'}"
                ></span>
              </span>
              <span class="text-sm font-medium"
                >{rooms_visibility_public()}</span
              >
            </button>
            <p class="text-xs opacity-60">
              {visibility === "public"
                ? rooms_visibility_public_hint()
                : rooms_visibility_private_hint()}
            </p>
          </div>

          {#if error}
            <p class="text-error-500 dark:text-error-400 text-xs">
              {rooms_create_failed({ error })}
            </p>
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
              type="submit"
              class="btn flex-1 preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110"
              disabled={!canSubmit}
            >
              {busy ? rooms_creating() : rooms_create_action()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
