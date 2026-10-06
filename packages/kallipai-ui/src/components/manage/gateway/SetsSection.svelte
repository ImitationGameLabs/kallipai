<script lang="ts">
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../../lib/classes.ts";
  // The collection detail's sets section: one card per member set in a
  // responsive grid (name / description / the default badge, and a
  // kebab carrying the set-as-default, edit, and remove actions).
  // The header row carries the create trigger (the SetCreateDialog
  // opens pre-bound to the open collection).
  // Prop-driven like the profiles section: mutations and the remove
  // confirmation live with the page, the card only reports intents.
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import ActionMenu from "../../ActionMenu.svelte";
  import { Pencil, Star, Trash } from "@lucide/svelte";
  import type { AdminSetRow } from "../../../lib/manage/gateway/client.ts";
  import {
    common_edit,
    common_remove,
    manage_gateway_sets,
    manage_gateway_set_add,
    manage_gateway_set_desc,
    manage_profiles_set_actions_aria,
    manage_profiles_set_as_default,
    manage_profiles_set_default_badge,
    manage_profiles_set_drop_here,
  } from "../../../paraglide/messages.js";

  let {
    sets,
    defaultSet,
    onEdit,
    onRemove,
    onAdd,
    onSetDefault,
    dragOverSet,
    onSetDragOver,
    onSetDragLeave,
    onSetDrop,
  }: {
    /** The open collection's member sets (empty while the first read runs). */
    sets: readonly AdminSetRow[];
    /** The collection's default-set anchor (null = none chosen yet). */
    defaultSet: string | null;
    onEdit: (set: AdminSetRow) => void;
    onRemove: (set: AdminSetRow) => void;
    onAdd: () => void;
    /** Transfer the default-set anchor to this member set. */
    onSetDefault: (set: AdminSetRow) => void;
    /** The set name under the drag cursor, for the drop highlight. */
    dragOverSet: string | null;
    onSetDragOver: (setName: string) => void;
    onSetDragLeave: (setName: string) => void;
    onSetDrop: (setName: string) => void;
  } = $props();
</script>

<section class="space-y-3">
  <div class="flex items-center justify-between gap-2">
    <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
      {manage_gateway_sets()}
    </h2>
    <button
      type="button"
      class="btn btn-sm preset-filled-primary-500"
      onclick={onAdd}
    >
      {manage_gateway_set_add()}
    </button>
  </div>
  <p class="text-xs opacity-60 mt-1">{manage_gateway_set_desc()}</p>
  <div class="grid grid-cols-1 sm:grid-cols-2 gap-3">
    {#each sets as set (set.name)}
      <div
        role="list"
        class="card preset-tonal-surface p-4 space-y-2 {dragOverSet === set.name
          ? 'outline-2 outline-dashed outline-primary-500'
          : ''}"
        ondragover={(e) => {
          e.preventDefault();
          onSetDragOver(set.name);
        }}
        ondragleave={() => onSetDragLeave(set.name)}
        ondrop={(e) => {
          e.preventDefault();
          onSetDrop(set.name);
        }}
      >
        {#if dragOverSet === set.name}
          <p class="text-xs opacity-80">{manage_profiles_set_drop_here()}</p>
        {/if}
        <div class="flex items-start justify-between gap-2">
          <div class="min-w-0">
            <div class="flex items-center gap-2">
              <span class="text-sm font-medium font-mono truncate">
                {set.name}
              </span>
              {#if set.name === defaultSet}
                <span class="badge preset-filled-primary-500 text-xs shrink-0">
                  {manage_profiles_set_default_badge()}
                </span>
              {/if}
            </div>
            {#if set.description}
              <p class="text-xs opacity-60 mt-0.5 truncate">
                {set.description}
              </p>
            {/if}
          </div>
          <ActionMenu
            label={manage_profiles_set_actions_aria()}
            onSelect={(value) => {
              if (value === "default") onSetDefault(set);
              else if (value === "edit") onEdit(set);
              else if (value === "remove") onRemove(set);
            }}
          >
            {#if set.name !== defaultSet}
              <Menu.Item value="default" class={MENU_ITEM}>
                <Star class="size-4" />
                {manage_profiles_set_as_default()}
              </Menu.Item>
            {/if}
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
