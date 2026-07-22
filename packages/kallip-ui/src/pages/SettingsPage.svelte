<script lang="ts">
  import { agoraSession } from "../lib/session/agora.svelte";
  import { sessionStore } from "../lib/session/session.svelte";
  import { configStore } from "../lib/config/config.svelte";
  import { modeOf } from "../lib/config/mode.ts";

  // Settings is now info-only: account actions (logout, mode switch) live in
  // the sidebar AccountMenu. Online shows the account (identity lives in
  // agora); offline shows the tagma connection (no identity). Offline
  // Disconnect/Reconnect stays here -- it is tagma session management, not an
  // account/mode action.
  const mode = $derived(modeOf(configStore.value));
  const offlineUrl = $derived(configStore.value?.offline?.tagmaUrl ?? "");

  // Offline: drop the tagma session without abandoning offline mode.
  function disconnect() {
    sessionStore.detach();
  }
</script>

<svelte:head><title>KallipAI · settings</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-md space-y-6">
    <h1 class="text-xl font-semibold">Settings</h1>

    {#if mode === "online"}
      {#if agoraSession.user}
        {@const me = agoraSession.user}
        <section class="space-y-3">
          <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
            Account
          </h2>
          <div class="card preset-tonal-surface p-4">
            <!-- display_name is nullable; fall back to the username handle when
                 unset (presentation policy lives here, not the data layer). -->
            <div class="min-w-0">
              <div class="text-sm font-medium truncate">
                {me.display_name ?? me.username}
              </div>
              <div class="text-xs opacity-60 font-mono break-all">
                {me.email}
              </div>
            </div>
          </div>
        </section>
      {/if}
    {:else}
      <section class="space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          Connection
        </h2>
        <div class="card preset-tonal-surface p-4 space-y-3">
          <div class="flex items-center gap-2 text-sm">
            <span
              class="size-2 rounded-full {sessionStore.connected
                ? 'bg-success-500'
                : 'bg-error-500'}"
              aria-hidden="true"
            ></span>
            <span class="font-medium"
              >{sessionStore.connected ? "Connected" : "Disconnected"}</span
            >
          </div>
          <div class="text-xs opacity-60 font-mono break-all">{offlineUrl}</div>
          <div class="flex flex-wrap gap-2">
            {#if sessionStore.connected}
              <button
                class="btn btn-sm preset-tonal-surface"
                onclick={disconnect}>Disconnect</button
              >
            {:else}
              <a href="/connect" class="btn btn-sm preset-filled-primary-500"
                >Reconnect</a
              >
            {/if}
          </div>
        </div>
      </section>
    {/if}
  </div>
</div>
