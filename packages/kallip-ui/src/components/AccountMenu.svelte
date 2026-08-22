<script lang="ts">
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import { ArrowRightLeft, LogOut, Settings, User } from "@lucide/svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { configStore } from "../lib/config/config.svelte";
  import { modeOf } from "../lib/config/mode.ts";
  import { connectionViewModel } from "../lib/connection.svelte.ts";
  import { navigate } from "../lib/shell/port.ts";
  import {
    logout,
    switchToOffline,
    switchToOnline,
  } from "../lib/session/account-actions.ts";
  import {
    account_go_offline,
    account_go_online,
    account_logout,
    settings_heading,
  } from "../paraglide/messages.js";

  // The sidebar-footer account menu: the desktop sidebar's single entry
  // point for identity + mode actions. Small screens replace this dropdown
  // with the /account hub page (the bottom bar's trailing cell links
  // there), so this component renders only the footer's wide pill
  // trigger. The action behavior lives in account-actions.ts (shared
  // with the hub page); only the trigger and menu markup live here.

  // Branch on mode, not on `user`: the agora session cookie survives offline
  // mode, so `user` can hold a stale MeResponse while offline (see the invariant
  // on `agoraSession.user`). Offline UI must never act on it.
  const mode = $derived(modeOf(configStore.value));
  const connection = $derived(
    connectionViewModel({
      connected: channelsStore.localConnected,
      connecting: false,
    }),
  );

  function onSelect(details: { value: string }) {
    switch (details.value) {
      case "settings":
        void navigate("/settings");
        break;
      case "logout":
        void logout();
        break;
      case "switch-online":
        void switchToOnline();
        break;
      case "switch-offline":
        void switchToOffline();
        break;
    }
  }
</script>

<!--
  The positioner is portaled to document.body so the upward-opening menu is
  not clipped by the shell's `overflow-hidden` grid (RootLayout) or the
  sidebar column. It opens top-start above the wide footer trigger.
-->
<Menu positioning={{ placement: "top-start", gutter: 8 }} {onSelect}>
  <Menu.Trigger
    class="w-full preset-tonal-surface hover:preset-filled-surface-500 px-2 py-1.5 rounded-base text-lg flex items-center gap-1.5"
  >
    {#if mode === "online" && agoraSession.user}
      <User class="size-4 shrink-0 opacity-70" />
      <span class="truncate opacity-80" title="@{agoraSession.user.username}"
        >@{agoraSession.user.username}</span
      >
    {:else}
      <User class="size-4 shrink-0 opacity-70" />
      <span
        class="size-2 rounded-full {connection.dotClass} shrink-0"
        aria-hidden="true"
      ></span>
      <span class="opacity-70 truncate">{connection.label}</span>
    {/if}
  </Menu.Trigger>
  <Portal>
    <Menu.Positioner>
      <Menu.Content class="card preset-tonal-surface p-1 min-w-[12rem]">
        <Menu.Item
          value="settings"
          class="flex items-center gap-2 px-3 py-2 rounded-base text-sm hover:preset-filled-surface-500 cursor-pointer"
        >
          <Settings class="size-4" />
          {settings_heading()}
        </Menu.Item>
        <Menu.Separator class="my-1 border-surface-200-800" />
        {#if mode === "online"}
          <Menu.Item
            value="logout"
            class="flex items-center gap-2 px-3 py-2 rounded-base text-sm hover:preset-filled-surface-500 cursor-pointer"
          >
            <LogOut class="size-4" />
            {account_logout()}
          </Menu.Item>
          <Menu.Separator class="my-1 border-surface-200-800" />
          <Menu.Item
            value="switch-offline"
            class="flex items-center gap-2 px-3 py-2 rounded-base text-sm hover:preset-filled-surface-500 cursor-pointer"
          >
            <ArrowRightLeft class="size-4" />
            {account_go_offline()}
          </Menu.Item>
        {:else}
          <Menu.Item
            value="switch-online"
            class="flex items-center gap-2 px-3 py-2 rounded-base text-sm hover:preset-filled-surface-500 cursor-pointer"
          >
            <ArrowRightLeft class="size-4" />
            {account_go_online()}
          </Menu.Item>
        {/if}
      </Menu.Content>
    </Menu.Positioner>
  </Portal>
</Menu>
