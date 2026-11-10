<script lang="ts" module>
  // Publish dialog: pick the audience and land one publish or
  // unpublish. The reserved audience is a built-in option (it is not a
  // row on the groups list); the duplicate-publish and not-published
  // cases surface as the server's error message.
</script>

<script lang="ts">
  import FormDialog from "../FormDialog.svelte";
  import { EVERYONE_GROUP_ID } from "../../lib/gateway/client.ts";
  import {
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
  function reseed(): void {
    groupDraft = EVERYONE_GROUP_ID;
  }

  const publishing = $derived(mode === "publish");
</script>

<FormDialog
  {open}
  title={publishing
    ? user_gateway_publish_title({ name: collection?.name ?? "" })
    : user_gateway_unpublish_title({ name: collection?.name ?? "" })}
  {busy}
  {error}
  canSubmit={collection !== null}
  submitLabel={publishing ? user_gateway_publish() : user_gateway_unpublish()}
  onOpen={reseed}
  onSubmit={() => onConfirm(groupDraft)}
  {onCancel}
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
</FormDialog>
