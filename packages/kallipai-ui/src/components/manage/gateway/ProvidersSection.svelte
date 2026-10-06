<script lang="ts" module>
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../../lib/classes.ts";
  // The provider pool section: one row per pool entry (id, family, base
  // URL, the key's mask), a kebab carrying the edit, credential, and
  // remove actions per row, and the create button. Mutations land
  // through the store; the dialogs are the section's only forms.
  import type { ProviderRow } from "../../../lib/manage/gateway/client.ts";
</script>

<script lang="ts">
  import ActionMenu from "../../ActionMenu.svelte";
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import { KeyRound, Pencil, Trash } from "@lucide/svelte";
  import {
    common_edit,
    common_remove,
    manage_gateway_provider_actions_aria,
    manage_gateway_provider_add,
    user_gateway_provider_credential,
  } from "../../../paraglide/messages.js";

  // The row actions are the shared kebab (the EnrollmentCodeCard
  // pattern): the card stays identity-only, actions one popover away.

  let {
    providers,
    onEdit,
    onRemove,
    onCreate,
    onCredential,
  }: {
    providers: ProviderRow[];
    onEdit: (row: ProviderRow) => void;
    onRemove: (row: ProviderRow) => void;
    onCreate: () => void;
    onCredential: (row: ProviderRow) => void;
  } = $props();
</script>

<div class="space-y-3">
  {#each providers as row (row.owner + "/" + row.provider_id)}
    <div class="card preset-tonal-surface p-3 text-sm space-y-1">
      <div class="flex items-center gap-3">
        <span class="min-w-0 flex-1 truncate font-mono">
          {row.provider_id}
        </span>
        <ActionMenu
          label={manage_gateway_provider_actions_aria()}
          onSelect={(value) => {
            if (value === "edit") onEdit(row);
            else if (value === "credential") onCredential(row);
            else if (value === "remove") onRemove(row);
          }}
        >
          <Menu.Item value="edit" class={MENU_ITEM}>
            <Pencil class="size-4" />
            {common_edit()}
          </Menu.Item>
          <Menu.Item value="credential" class={MENU_ITEM}>
            <KeyRound class="size-4" />
            {user_gateway_provider_credential()}
          </Menu.Item>
          <Menu.Separator class="my-1 border-t border-surface-300" />
          <Menu.Item value="remove" class={MENU_ITEM_DANGER}>
            <Trash class="size-4" />
            {common_remove()}
          </Menu.Item>
        </ActionMenu>
      </div>
      <p class="text-xs opacity-60 truncate">{row.family}</p>
      <p class="text-xs opacity-60 truncate">{row.base_url ?? ""}</p>
      <p class="text-xs opacity-60 truncate font-mono">
        {row.api_key_masked ?? ""}
      </p>
    </div>
  {/each}
  <button
    class="btn btn-sm preset-filled-primary-500"
    onclick={() => onCreate()}
  >
    {manage_gateway_provider_add()}
  </button>
</div>
