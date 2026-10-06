<script lang="ts">
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../lib/classes.ts";
  // The caller's profiles section: one card per row (model, provider,
  // and the parked/imported badges) with edit and remove in the kebab.
  // Prop-driven like the admin section.
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import ActionMenu from "../ActionMenu.svelte";
  import { Pencil, Trash } from "@lucide/svelte";
  import type { UserProfileRow } from "../../lib/gateway/client.ts";
  import {
    common_edit,
    common_remove,
    user_gateway_profile_add,
    user_gateway_profile_parked,
    user_gateway_profiles,
  } from "../../paraglide/messages.js";

  let {
    profiles,
    onEdit,
    onRemove,
    onCreate,
  }: {
    /** The caller's profile rows (empty while the first read runs). */
    profiles: readonly UserProfileRow[];
    onEdit: (row: UserProfileRow) => void;
    onRemove: (row: UserProfileRow) => void;
    onCreate: () => void;
  } = $props();
</script>

<section class="space-y-3">
  <div class="flex items-center justify-between gap-2">
    <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
      {user_gateway_profiles()}
    </h2>
    <button
      type="button"
      class="btn text-xs preset-outlined-surface-500 hover:preset-filled-surface-500"
      onclick={onCreate}
    >
      + {user_gateway_profile_add()}
    </button>
  </div>
  <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
    {#each profiles as row (row.profile_id)}
      <div class="card preset-tonal-surface p-4 space-y-2">
        <div class="flex items-start justify-between gap-2">
          <div class="min-w-0">
            <div class="flex items-center gap-2">
              <span class="text-sm font-medium font-mono truncate">
                {row.profile_id}
              </span>
              {#if row.parked}
                <span class="badge preset-filled-surface-500 text-xs shrink-0">
                  {user_gateway_profile_parked()}
                </span>
              {/if}
            </div>
            <p class="text-xs opacity-60 mt-0.5 truncate">
              {row.model} · {row.provider_id}
            </p>
          </div>
          <ActionMenu
            label={user_gateway_profiles()}
            onSelect={(value) => {
              if (value === "edit") onEdit(row);
              else if (value === "remove") onRemove(row);
            }}
          >
            <Menu.Item value="edit" class={MENU_ITEM}>
              <Pencil class="size-4" />
              {common_edit()}
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
