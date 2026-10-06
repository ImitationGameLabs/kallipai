<script lang="ts" module>
  // Publish dialog: pick the audience and land one publish or
  // unpublish. The reserved audience is a built-in option (it is not a
  // row on the groups list); the duplicate-publish and not-published
  // cases surface as the server's error message.
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import { EVERYONE_GROUP_ID } from "../../lib/gateway/client.ts";
  import {
    common_cancel,
    user_gateway_everyone,
    user_gateway_publish,
    user_gateway_publish_group,
    user_gateway_publish_title,
    user_gateway_unpublish,
    user_gateway_unpublish_title,
  } from "../../paraglide/messages.js";

  let {
    open,
    collection,
    groups,
    mode = "publish",
    busy = false,
    error = null,
    onConfirm,
    onCancel,
  }: {
    open: boolean;
    /** The collection being published or withdrawn. */
    collection: { name: string } | null;
    /** The caller's groups (the reserved audience rides on top). */
    groups: readonly { group_id: string; name: string }[];
    mode?: "publish" | "unpublish";
    busy?: boolean;
    error?: string | null;
    onConfirm: (groupId: string) => void;
    onCancel: () => void;
  } = $props();

  let groupDraft = $state(EVERYONE_GROUP_ID);
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      groupDraft = EVERYONE_GROUP_ID;
    }
    lastOpen = open;
  });

  const publishing = $derived(mode === "publish");

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
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
          {publishing
            ? user_gateway_publish_title({ name: collection?.name ?? "" })
            : user_gateway_unpublish_title({ name: collection?.name ?? "" })}
        </Dialog.Title>

        <form
          class="flex flex-col gap-3"
          onsubmit={(e) => {
            e.preventDefault();
            onConfirm(groupDraft);
          }}
        >
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {user_gateway_publish_group()}
            </span>
            <select class="select text-sm" bind:value={groupDraft}>
              <option value={EVERYONE_GROUP_ID}>
                {user_gateway_everyone()}
              </option>
              {#each groups as group (group.group_id)}
                <option value={group.group_id}>{group.name}</option>
              {/each}
            </select>
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
              disabled={busy || collection === null}
            >
              {publishing ? user_gateway_publish() : user_gateway_unpublish()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
