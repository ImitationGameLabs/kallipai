<script lang="ts">
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../lib/classes.ts";
  // The caller's collections section: one card per collection (name,
  // description, set count) with publish, edit, and remove in the
  // kebab. Prop-driven like the sibling sections.
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import ActionMenu from "../ActionMenu.svelte";
  import { Globe, Pencil, Trash } from "@lucide/svelte";
  import type { UserCollectionRow } from "../../lib/gateway/client.ts";
  import {
    common_edit,
    common_remove,
    user_gateway_collection_add,
    user_gateway_collections,
    user_gateway_publish,
  } from "../../paraglide/messages.js";

  let {
    collections,
    onEdit,
    onRemove,
    onPublish,
    onCreate,
  }: {
    /** The caller's collections (empty while the first read runs). */
    collections: readonly UserCollectionRow[];
    onEdit: (row: UserCollectionRow) => void;
    onRemove: (row: UserCollectionRow) => void;
    onPublish: (row: UserCollectionRow) => void;
    onCreate: () => void;
  } = $props();
</script>

<section class="space-y-3">
  <div class="flex items-center justify-between gap-2">
    <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
      {user_gateway_collections()}
    </h2>
    <button
      type="button"
      class="btn text-xs preset-outlined-surface-500 hover:preset-filled-surface-500"
      onclick={onCreate}
    >
      + {user_gateway_collection_add()}
    </button>
  </div>
  <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
    {#each collections as row (row.name)}
      <div class="card preset-tonal-surface p-4 space-y-2">
        <div class="flex items-start justify-between gap-2">
          <div class="min-w-0">
            <span class="text-sm font-medium font-mono truncate">
              {row.name}
            </span>
            {#if row.description}
              <p class="text-xs opacity-60 mt-0.5 truncate">
                {row.description}
              </p>
            {/if}
            <p class="text-xs opacity-60 mt-0.5">
              {row.sets.length}
            </p>
          </div>
          <ActionMenu
            label={user_gateway_collections()}
            onSelect={(value) => {
              if (value === "publish") onPublish(row);
              else if (value === "edit") onEdit(row);
              else if (value === "remove") onRemove(row);
            }}
          >
            <Menu.Item value="publish" class={MENU_ITEM}>
              <Globe class="size-4" />
              {user_gateway_publish()}
            </Menu.Item>
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
