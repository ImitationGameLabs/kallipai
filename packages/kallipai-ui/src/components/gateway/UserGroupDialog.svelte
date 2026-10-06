<script lang="ts" module>
  // Group edit dialog: the name and the member accounts. The dialog
  // holds a local draft seeded on each open transition; the page lands
  // it as one create or update. The reserved audience name is refused
  // server-side; the dialog forwards and surfaces the reason.
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import ReqMark from "../ReqMark.svelte";
  import {
    common_cancel,
    common_save,
    user_gateway_group_add,
    user_gateway_group_edit_title,
    user_gateway_group_members,
    user_gateway_group_name,
  } from "../../paraglide/messages.js";

  let {
    open,
    row,
    busy = false,
    error = null,
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The group under edit (null = the create form). */
    row: { group_id: string; name: string; members: string[] } | null;
    busy?: boolean;
    error?: string | null;
    onSave: (result: { name: string; members: string[] }) => void;
    onCancel: () => void;
  } = $props();

  let nameDraft = $state("");
  let membersDraft = $state("");
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      nameDraft = row?.name ?? "";
      membersDraft = row ? row.members.join("\n") : "";
    }
    lastOpen = open;
  });

  const canSubmit = $derived(nameDraft.trim() !== "");

  function parseMembers(raw: string): string[] {
    return raw
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line !== "");
  }

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }

  function submit(): void {
    if (!canSubmit || busy) return;
    onSave({ name: nameDraft.trim(), members: parseMembers(membersDraft) });
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
          {row === null
            ? user_gateway_group_add()
            : user_gateway_group_edit_title()}
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
              {user_gateway_group_name()}
              <ReqMark />
            </span>
            <input
              class="input text-sm"
              bind:value={nameDraft}
              aria-required="true"
            />
          </label>
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {user_gateway_group_members()}
            </span>
            <textarea
              class="textarea text-sm font-mono"
              rows="5"
              bind:value={membersDraft}></textarea>
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
