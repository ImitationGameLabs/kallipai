<script lang="ts">
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../../lib/classes.ts";
  // The unassigned section: one row per unassigned profile -- the whole
  // catalog space's, or one collection's when the panel is scoped to a
  // detail. An unassigned profile is a profile row outside every set:
  // the filter is the wire flag, not a separate store. A row's identity
  // is its collection plus its id (the id alone repeats across
  // collections), so both the each key and the row carry the collection.
  // a separate store. A row's identity is its collection plus its id
  // (the id alone repeats across collections), so both the each key and
  // the row carry the collection.
  // The header row carries the create trigger (the ProfileDialog).
  // Prop-driven like the sets section: the filter and the mutations
  // live with the store, the rows only report intents.
  // Every row drags: dropping it on a set makes it a member (the
  // server promotes the row in the same write). The drag carries the
  // row, not just the id: the collection decides where it can land.
  import type { AdminProfileRow } from "../../../lib/manage/gateway/client.ts";
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import ActionMenu from "../../ActionMenu.svelte";
  import { Pencil, Trash } from "@lucide/svelte";
  import {
    common_edit,
    common_remove,
    manage_gateway_unassigned,
    manage_gateway_unassigned_actions_aria,
    manage_gateway_unassigned_desc,
    manage_gateway_unassigned_desc_collection,
    manage_gateway_unassigned_add,
  } from "../../../paraglide/messages.js";

  let {
    unassigned,
    scope = null,
    onCreate,
    onEdit,
    onDelete,
    onDragStart,
    onDragEnd,
  }: {
    /** The unassigned profiles on show: the whole space's, or one collection's. */
    unassigned: readonly AdminProfileRow[];
    /** The collection the panel is narrowed to (null = the whole space). */
    scope?: string | null;
    onCreate: () => void;
    onEdit: (profile: AdminProfileRow) => void;
    onDelete: (profile: AdminProfileRow) => void;
    onDragStart: (profile: AdminProfileRow) => void;
    onDragEnd: () => void;
  } = $props();
</script>

<section class="space-y-3">
  <div class="flex items-center justify-between gap-2">
    <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
      {manage_gateway_unassigned()}
    </h2>
    <button
      type="button"
      class="btn btn-sm preset-filled-primary-500"
      onclick={onCreate}
    >
      {manage_gateway_unassigned_add()}
    </button>
  </div>
  <p class="text-xs opacity-60 mt-1">
    {scope === null
      ? manage_gateway_unassigned_desc()
      : manage_gateway_unassigned_desc_collection({ collection: scope })}
  </p>
  <div role="list" class="space-y-3">
    {#each unassigned as p (`${p.collection_name}/${p.profile_id}`)}
      <div
        role="listitem"
        class="card preset-tonal-surface p-3 flex items-center gap-3 cursor-grab"
        draggable={true}
        ondragstart={(e) => {
          // Firefox only starts a drag session if dataTransfer gets data.
          if (e.dataTransfer) {
            e.dataTransfer.setData("text/plain", p.profile_id);
            e.dataTransfer.effectAllowed = "move";
          }
          onDragStart(p);
        }}
        ondragend={onDragEnd}
      >
        <span class="min-w-0 flex-1 flex items-baseline gap-2 text-sm">
          <span class="truncate">{p.profile_id}</span>
          <span class="text-xs opacity-60 font-mono truncate shrink-0"
            >{p.collection_name}</span
          >
        </span>
        <span class="text-xs opacity-60 font-mono truncate"
          >{p.provider_id}</span
        >
        <ActionMenu
          label={manage_gateway_unassigned_actions_aria()}
          onSelect={(value) => {
            if (value === "edit") onEdit(p);
            else if (value === "remove") onDelete(p);
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
    {/each}
  </div>
</section>
