<script lang="ts">
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import { ArrowRightLeft, LogOut, Settings, User } from "@lucide/svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { roomsStore } from "../lib/session/rooms.svelte";
  import { roomConversationsStore } from "../lib/session/roomConversations.svelte";
  import { configStore } from "../lib/config/config.svelte";
  import { connectDirect } from "../lib/session/connect.ts";
  import { navigate } from "../lib/shell/port.ts";
  import { modeOf } from "../lib/config/mode.ts";
  import { connectionViewModel } from "../lib/connection.svelte.ts";

  // The account menu is the single entry point for identity + mode actions.
  // It renders in the sidebar footer (online and offline alike): online shows
  // the signed-in @handle, offline shows a connection-status dot + label. Both
  // branches lead with the same User icon so the trigger silhouette is stable.

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

  // Online: end the agora session (destroys the cookie -- distinct from
  // switching, which keeps it). Drop open channels here; the realtime SSE that
  // fed them is torn down separately by RootLayout's $effect when `user` flips
  // to null (no 401 reconnect churn). The gate then sees user===null and
  // redirects to /login (it owns the navigation), so no manual navigate here.
  async function logout() {
    channelsStore.reset();
    // Drop the per-user room registry + rendered room transcripts: they are
    // plaintext and keyed on the leaving user, so they must not linger into the
    // next session on a shared device.
    roomsStore.reset();
    roomConversationsStore.reset();
    await agoraSession.logout();
  }

  // Offline -> online: detach the tagma and flip the active mode. The agora
  // session cookie survives offline mode (we never logout() on a switch), so a
  // whoami() re-resolves the signed-in user with no re-auth. The retained
  // offline creds stay on disk for the switch back. Non-destructive, so no
  // confirm. The gate owns post-switch routing.
  async function switchToOnline() {
    channelsStore.detachLocal();
    await configStore.setActiveMode("online");
    void agoraSession.whoami();
  }

  // Online -> offline: if offline creds are already saved, reconnect to the
  // tagma directly (re-auth-free); otherwise send the user to /connect for
  // first-time setup. Drop open channels: offline mode does not render online
  // chats, so their SSE subscriber would keep running (against the still-valid
  // cookie) and update transcripts nobody sees. The race guard re-checks
  // activeMode before attachLocal: if the user flipped back to online while the
  // connect was in flight, close the stray transport instead of attaching it
  // (avoids a held tagma transport).
  async function switchToOffline() {
    // Mode switch: tear down transports but PRESERVE the IndexedDB cache so the
    // offline path rehydrates from the same rows the online path wrote.
    channelsStore.tearDownAll();
    const offline = configStore.value?.offline;
    if (!offline) {
      await navigate("/connect");
      return;
    }
    await configStore.setActiveMode("offline");
    let connection;
    try {
      connection = await connectDirect(offline);
    } catch (e) {
      channelsStore.localError = e;
      return;
    }
    if (configStore.value?.activeMode === "offline") {
      await channelsStore.attachLocal(
        connection.transport,
        connection.conversationId,
      );
    } else {
      // Mode flipped before attach landed: tear down the transport we opened.
      // close() is synchronous (it only aborts the SSE fetch).
      connection.transport.close();
    }
  }

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
  The positioner is portaled to document.body so the upward-opening menu is not
  clipped by the shell's `overflow-hidden` grid (RootLayout) or the sidebar
  column. `placement: "top-start"` opens it above the footer trigger.
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
          Settings
        </Menu.Item>
        <Menu.Separator class="my-1 border-surface-200-800" />
        {#if mode === "online"}
          <Menu.Item
            value="logout"
            class="flex items-center gap-2 px-3 py-2 rounded-base text-sm hover:preset-filled-surface-500 cursor-pointer"
          >
            <LogOut class="size-4" />
            Log out
          </Menu.Item>
          <Menu.Separator class="my-1 border-surface-200-800" />
          <Menu.Item
            value="switch-offline"
            class="flex items-center gap-2 px-3 py-2 rounded-base text-sm hover:preset-filled-surface-500 cursor-pointer"
          >
            <ArrowRightLeft class="size-4" />
            Go offline
          </Menu.Item>
        {:else}
          <Menu.Item
            value="switch-online"
            class="flex items-center gap-2 px-3 py-2 rounded-base text-sm hover:preset-filled-surface-500 cursor-pointer"
          >
            <ArrowRightLeft class="size-4" />
            Go online
          </Menu.Item>
        {/if}
      </Menu.Content>
    </Menu.Positioner>
  </Portal>
</Menu>
