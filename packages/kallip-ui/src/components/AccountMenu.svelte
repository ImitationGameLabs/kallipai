<script lang="ts">
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import { ArrowRightLeft, LogOut, Settings, User } from "@lucide/svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { sessionStore } from "../lib/session/session.svelte";
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
  const connection = $derived(connectionViewModel(sessionStore));

  // Online: end the agora session (destroys the cookie -- distinct from
  // switching, which keeps it). Drop open channels here; the realtime SSE that
  // fed them is torn down separately by RootLayout's $effect when `user` flips
  // to null (no 401 reconnect churn). The gate then sees user===null and
  // redirects to /login (it owns the navigation), so no manual navigate here.
  async function logout() {
    channelsStore.reset();
    await agoraSession.logout();
  }

  // Offline -> online: detach the tagma and flip the active mode. The agora
  // session cookie survives offline mode (we never logout() on a switch), so a
  // whoami() re-resolves the signed-in user with no re-auth. The retained
  // offline creds stay on disk for the switch back. Non-destructive, so no
  // confirm. The gate owns post-switch routing.
  async function switchToOnline() {
    sessionStore.detach();
    await configStore.setActiveMode("online");
    void agoraSession.whoami();
  }

  // Online -> offline: if offline creds are already saved, reconnect to the
  // tagma directly (re-auth-free); otherwise send the user to /connect for
  // first-time setup. Drop open channels: offline mode does not render /chat, so
  // their SSE subscriber would keep running (against the still-valid cookie) and
  // update transcripts nobody sees. The race guard re-checks activeMode before
  // attach: if the user flipped back to online while the connect was in flight,
  // close the stray session instead of attaching it (avoids a held tagma
  // transport).
  async function switchToOffline() {
    channelsStore.reset();
    const offline = configStore.value?.offline;
    if (!offline) {
      await navigate("/connect");
      return;
    }
    await configStore.setActiveMode("offline");
    let session;
    try {
      session = await connectDirect(offline);
    } catch (e) {
      sessionStore.error = e;
      return;
    }
    if (configStore.value?.activeMode === "offline") {
      await sessionStore.attach(session);
    } else {
      session.close().catch(() => {});
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
        <Menu.Separator class="my-1 border-surface-200" />
        {#if mode === "online"}
          <Menu.Item
            value="logout"
            class="flex items-center gap-2 px-3 py-2 rounded-base text-sm hover:preset-filled-surface-500 cursor-pointer"
          >
            <LogOut class="size-4" />
            Log out
          </Menu.Item>
          <Menu.Separator class="my-1 border-surface-200" />
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
