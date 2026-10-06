<script lang="ts">
  import { MENU_ITEM, MENU_ITEM_DANGER } from "../../../lib/classes.ts";
  // The parking section: one row per parked profile in the catalog
  // space -- the one space this page manages (full CRUD: the kebab
  // carries the edit and remove actions).
  // The header row carries the create trigger (the ProfileDialog).
  // Prop-driven like the sets section: the filter and the mutations
  // live with the store, the rows only report intents.
  // Every row drags: dropping it on a set makes it a member (the
  // server promotes the row in the same write).
  import type { AdminProfileRow } from "../../../lib/manage/gateway/client.ts";
  import { Menu } from "@skeletonlabs/skeleton-svelte";
  import ActionMenu from "../../ActionMenu.svelte";
  import { Pencil, Trash } from "@lucide/svelte";
  import {
    common_edit,
    common_remove,
    manage_gateway_parking,
    manage_gateway_parking_actions_aria,
    manage_gateway_parking_desc,
    manage_profiles_parking_add,
  } from "../../../paraglide/messages.js";

  let {
    parking,
    onCreate,
    onEdit,
    onDelete,
    onDragStart,
    onDragEnd,
  }: {
    /** The parked profile rows (the catalog space's drafts). */
    parking: readonly AdminProfileRow[];
    onCreate: () => void;
    onEdit: (profile: AdminProfileRow) => void;
    onDelete: (profile: AdminProfileRow) => void;
    onDragStart: (profileId: string) => void;
    onDragEnd: () => void;
  } = $props();
</script>

<section class="space-y-3">
  <div class="flex items-center justify-between gap-2">
    <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
      {manage_gateway_parking()}
    </h2>
    <button
      type="button"
      class="btn btn-sm preset-filled-primary-500"
      onclick={onCreate}
    >
      {manage_profiles_parking_add()}
    </button>
  </div>
  <p class="text-xs opacity-60 mt-1">{manage_gateway_parking_desc()}</p>
  <div role="list" class="space-y-3">
    {#each parking as p (p.profile_id)}
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
          onDragStart(p.profile_id);
        }}
        ondragend={onDragEnd}
      >
        <span class="min-w-0 flex-1 truncate text-sm">{p.profile_id}</span>
        <span class="text-xs opacity-60 font-mono truncate"
          >{p.provider_id}</span
        >
        <ActionMenu
          label={manage_gateway_parking_actions_aria()}
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
