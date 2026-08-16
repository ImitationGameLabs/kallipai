<script lang="ts" module>
  // A room list entry: the room's name + description + visibility + a kebab menu
  // that opens the room's settings page (where invite/add-tagma/leave live). A
  // clean list row, not the management surface it used to be. The body opens the
  // conversation; the kebab opens settings. Prop-driven throughout.
  import type { RoomView } from "@kallipai/kallip-lesche-client";

  export interface RoomCardProps {
    readonly room: RoomView;
    /** Open the room's conversation view. */
    onOpen?: () => void;
    /** Open the room's settings page. */
    onSettings?: () => void;
  }
</script>

<script lang="ts">
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import { MoreVertical, Settings } from "@lucide/svelte";
  import { getLocale } from "../../paraglide/runtime.js";
  import {
    room_label_fallback,
    room_public_badge,
    room_actions_aria,
    rooms_menu_settings,
  } from "../../paraglide/messages.js";

  let { room, onOpen, onSettings }: RoomCardProps = $props();

  // The human label: the room name, falling back to a short id prefix for rooms
  // created before names existed (or seeded empty). The full id is shown muted.
  const name = $derived(
    room.name || room_label_fallback({ id: room.room_id.slice(0, 8) }),
  );
  const isPublic = $derived(room.visibility === "public");
</script>

<div
  class="card preset-tonal-surface transition hover:brightness-95 flex items-center gap-3 p-4"
>
  <button
    type="button"
    class="flex-1 min-w-0 text-left flex flex-col gap-1"
    onclick={onOpen}
  >
    <div class="flex items-center gap-2 min-w-0">
      <span class="font-semibold truncate">{name}</span>
      {#if isPublic}
        <span
          class="text-xs preset-tonal-surface px-2 py-0.5 rounded-base shrink-0"
          >{room_public_badge()}</span
        >
      {/if}
    </div>
    {#if room.description}
      <p class="text-xs opacity-70 line-clamp-2">{room.description}</p>
    {/if}
    <p class="text-xs opacity-50">
      {new Date(room.created_at).toLocaleDateString(getLocale())}
    </p>
  </button>

  {#if onSettings}
    <div class="shrink-0">
      <Menu
        positioning={{ placement: "bottom-end" }}
        onSelect={(e) => {
          if (e.value === "settings") onSettings();
        }}
      >
        <Menu.Trigger
          class="size-8 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-surface-500"
          aria-label={room_actions_aria()}
        >
          <MoreVertical class="size-4" />
        </Menu.Trigger>
        <Portal>
          <Menu.Positioner>
            <Menu.Content class="card preset-tonal-surface p-1 min-w-[8rem]">
              <Menu.Item
                value="settings"
                class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
              >
                <Settings class="size-4" />
                {rooms_menu_settings()}
              </Menu.Item>
            </Menu.Content>
          </Menu.Positioner>
        </Portal>
      </Menu>
    </div>
  {/if}
</div>
