<script lang="ts">
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../../lib/classes.ts";
  // The platform groups section: one card per audience group (name /
  // member count) with a kebab carrying the edit and remove actions.
  // The everyone sentinel renders read-only: it is the platform's
  // reserved audience, the server answers 409 to any mutation, so the
  // card exposes no edit or remove entry for it. Prop-driven like the
  // collections section: mutations live with the page.
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import ActionMenu from "../../ActionMenu.svelte";
  import { Pencil, Trash } from "@lucide/svelte";
  import type { GroupRow } from "../../../lib/manage/gateway/client.ts";
  import {
    common_edit,
    common_remove,
    manage_gateway_group_actions_aria,
    manage_gateway_group_add,
    manage_gateway_group_members_count_one,
    manage_gateway_group_members_count_other,
    manage_gateway_group_platform_audience,
    manage_gateway_groups,
  } from "../../../paraglide/messages.js";

  let {
    groups,
    onEdit,
    onRemove,
    onAdd,
  }: {
    /** The platform groups (empty while the first read runs). */
    groups: readonly GroupRow[];
    onEdit: (group: GroupRow) => void;
    onRemove: (group: GroupRow) => void;
    onAdd: () => void;
  } = $props();
</script>

<section class="space-y-3">
  <div class="flex items-center justify-between gap-2">
    <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
      {manage_gateway_groups()}
    </h2>
    <button
      type="button"
      class="btn btn-sm preset-filled-primary-500"
      onclick={onAdd}
    >
      {manage_gateway_group_add()}
    </button>
  </div>
  <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
    {#each groups as group (group.group_id)}
      <div class="card preset-tonal-surface p-4 space-y-2">
        <div class="flex items-start justify-between gap-2">
          <div class="min-w-0">
            <span class="text-sm font-medium font-mono truncate">
              {group.name}
            </span>
            {#if group.group_id === "everyone"}
              <span
                class="ml-2 text-xs px-1.5 py-0.5 rounded-base preset-outlined-primary-500"
              >
                {manage_gateway_group_platform_audience()}
              </span>
            {/if}
            <p class="text-xs opacity-60 mt-0.5">
              {group.members.length === 1
                ? manage_gateway_group_members_count_one({
                    count: group.members.length,
                  })
                : manage_gateway_group_members_count_other({
                    count: group.members.length,
                  })}
            </p>
          </div>
          {#if group.group_id !== "everyone"}
            <ActionMenu
              label={manage_gateway_group_actions_aria()}
              contentClass="card preset-tonal-surface p-1 min-w-[10rem]"
              onSelect={(value) => {
                if (value === "edit") onEdit(group);
                else if (value === "remove") onRemove(group);
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
          {/if}
        </div>
      </div>
    {/each}
  </div>
</section>
