<script lang="ts">
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../lib/classes.ts";
  // The caller's groups section: one card per group (name, member
  // count) with edit and remove in the kebab. The reserved audience is
  // not listed here (it has no row); the publish dialog carries it as
  // a built-in option.
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import ActionMenu from "../ActionMenu.svelte";
  import { Pencil, Trash } from "@lucide/svelte";
  import type { UserGroupRow } from "../../lib/gateway/client.ts";
  import {
    common_edit,
    common_remove,
    user_gateway_group_add,
    user_gateway_groups,
  } from "../../paraglide/messages.js";

  let {
    groups,
    onEdit,
    onRemove,
    onCreate,
  }: {
    /** The caller's groups (empty while the first read runs). */
    groups: readonly UserGroupRow[];
    onEdit: (row: UserGroupRow) => void;
    onRemove: (row: UserGroupRow) => void;
    onCreate: () => void;
  } = $props();
</script>

<section class="space-y-3">
  <div class="flex items-center justify-between gap-2">
    <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
      {user_gateway_groups()}
    </h2>
    <button
      type="button"
      class="btn text-xs preset-outlined-surface-500 hover:preset-filled-surface-500"
      onclick={onCreate}
    >
      + {user_gateway_group_add()}
    </button>
  </div>
  <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
    {#each groups as row (row.group_id)}
      <div class="card preset-tonal-surface p-4 space-y-2">
        <div class="flex items-start justify-between gap-2">
          <div class="min-w-0">
            <span class="text-sm font-medium truncate">
              {row.name}
            </span>
            <p class="text-xs opacity-60 mt-0.5">
              {row.members.length}
            </p>
          </div>
          <ActionMenu
            label={user_gateway_groups()}
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
