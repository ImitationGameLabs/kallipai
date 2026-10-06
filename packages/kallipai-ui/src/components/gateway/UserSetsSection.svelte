<script lang="ts">
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../lib/classes.ts";
  // The caller's sets section, grouped by collection: one group per
  // collection (the group header carries the collection's name and a
  // create trigger pre-bound to that collection), and inside it one
  // card per member set (name, description, the member count) with
  // edit and remove in the kebab. Every set is born inside its
  // collection; the create never picks a target. Prop-driven like the
  // sibling sections.
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import ActionMenu from "../ActionMenu.svelte";
  import { Pencil, Trash } from "@lucide/svelte";
  import type {
    UserCollectionRow,
    UserSetRow,
  } from "../../lib/gateway/client.ts";
  import { memberSetsOf } from "../../lib/gateway/collections.ts";
  import {
    common_edit,
    common_remove,
    user_gateway_set_add,
    user_gateway_sets,
  } from "../../paraglide/messages.js";

  let {
    sets,
    collections,
    onEdit,
    onRemove,
    onCreate,
  }: {
    /** The caller's sets (empty while the first read runs). */
    sets: readonly UserSetRow[];
    /** The caller's collections: the groups, each with member names. */
    collections: readonly UserCollectionRow[];
    onEdit: (set: UserSetRow) => void;
    onRemove: (set: UserSetRow) => void;
    /** Open the create dialog bound to this collection. */
    onCreate: (collection: string) => void;
  } = $props();

  const setsIn = (collection: UserCollectionRow): UserSetRow[] =>
    memberSetsOf(sets, collection.sets);
</script>

<section class="space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
    {user_gateway_sets()}
  </h2>
  {#each collections as collection (collection.name)}
    <div class="space-y-2">
      <div class="flex items-center justify-between gap-2">
        <h3
          class="text-xs font-mono font-medium uppercase opacity-50 tracking-wide truncate"
        >
          {collection.name}
        </h3>
        <button
          type="button"
          class="btn text-xs preset-outlined-surface-500 hover:preset-filled-surface-500"
          onclick={() => onCreate(collection.name)}
        >
          + {user_gateway_set_add()}
        </button>
      </div>
      <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
        {#each setsIn(collection) as set (set.name)}
          <div class="card preset-tonal-surface p-4 space-y-2">
            <div class="flex items-start justify-between gap-2">
              <div class="min-w-0">
                <span class="text-sm font-medium font-mono truncate">
                  {set.name}
                </span>
                {#if set.description}
                  <p class="text-xs opacity-60 mt-0.5 truncate">
                    {set.description}
                  </p>
                {/if}
              </div>
              <ActionMenu
                label={user_gateway_sets()}
                onSelect={(value) => {
                  if (value === "edit") onEdit(set);
                  else if (value === "remove") onRemove(set);
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
    </div>
  {/each}
</section>
