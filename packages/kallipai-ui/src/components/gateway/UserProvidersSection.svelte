<script lang="ts">
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../lib/classes.ts";
  // The caller's provider pool section: one card per row (identity,
  // base URL, and the key mask), a kebab carrying edit, credential,
  // and remove. Prop-driven like the admin section: mutations and the
  // remove confirmation live with the page, the card only reports
  // intents.
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import ActionMenu from "../ActionMenu.svelte";
  import { KeyRound, Pencil, Trash } from "@lucide/svelte";
  import type { UserProviderRow } from "../../lib/gateway/client.ts";
  import {
    common_edit,
    common_remove,
    user_gateway_provider_add,
    user_gateway_provider_credential,
    user_gateway_providers,
  } from "../../paraglide/messages.js";

  let {
    providers,
    onEdit,
    onCredential,
    onRemove,
    onCreate,
  }: {
    /** The caller's pool rows (empty while the first read runs). */
    providers: readonly UserProviderRow[];
    onEdit: (row: UserProviderRow) => void;
    onCredential: (row: UserProviderRow) => void;
    onRemove: (row: UserProviderRow) => void;
    onCreate: () => void;
  } = $props();
</script>

<section class="space-y-3">
  <div class="flex items-center justify-between gap-2">
    <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
      {user_gateway_providers()}
    </h2>
    <button
      type="button"
      class="btn text-xs preset-outlined-surface-500 hover:preset-filled-surface-500"
      onclick={onCreate}
    >
      + {user_gateway_provider_add()}
    </button>
  </div>
  <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
    {#each providers as row (row.provider_id)}
      <div class="card preset-tonal-surface p-4 space-y-2">
        <div class="flex items-start justify-between gap-2">
          <div class="min-w-0">
            <span class="text-sm font-medium font-mono truncate">
              {row.provider_id}
            </span>
            <p class="text-xs opacity-60 mt-0.5 truncate">
              {row.family}{row.base_url ? ` · ${row.base_url}` : ""}
            </p>
            {#if row.api_key_masked}
              <p class="text-xs opacity-60 mt-0.5 font-mono">
                {row.api_key_masked}
              </p>
            {/if}
          </div>
          <ActionMenu
            label={user_gateway_providers()}
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
            <Menu.Item value="remove" class={MENU_ITEM_DANGER}>
              <Trash class="size-4" />
              {common_remove()}
            </Menu.Item>
          </ActionMenu>
        </div>
      </div>
    {/each}
  </div>
</section>
