<script lang="ts">
  // The /account landing page: the small-viewport twin of the sidebar
  // footer's AccountMenu dropdown — one full-width row per account entry,
  // reached from the bottom bar's trailing (account) cell. The desktop
  // sidebar keeps its dropdown; this page stays reachable by URL there,
  // mirroring how /local/manage relates to the sidebar's manage section.
  // Rows mix links (settings) and mode actions: an action has no
  // destination, so it is a button (not an anchor) and must not push a
  // history entry. Icons are imported directly here (not injected via
  // NavIcons) because page components already depend on @lucide/svelte
  // directly (ManageHubPage precedent).
  import { ArrowRightLeft, LogOut, Settings } from "@lucide/svelte";
  import { configStore } from "../../lib/config/config.svelte";
  import { modeOf } from "../../lib/config/mode.ts";
  import {
    logout,
    switchToOffline,
    switchToOnline,
  } from "../../lib/session/account-actions.ts";
  import {
    account_go_offline,
    account_go_online,
    account_logout,
    account_menu,
    settings_heading,
  } from "../../paraglide/messages.js";

  // Branch on mode, not on `user` (the AccountMenu invariant): offline must
  // never act on a stale agora session, so the row set follows the mode.
  const mode = $derived(modeOf(configStore.value));
</script>

<div class="px-2 py-4 md:p-6 max-w-2xl space-y-6">
  <h1 class="text-xl font-semibold">{account_menu()}</h1>

  <!-- Row order mirrors the dropdown: settings, then the mode actions. -->
  <nav
    class="card preset-tonal-surface divide-y divide-surface-200-800"
    aria-label={account_menu()}
  >
    <!-- One destination per row, full-width with the same 48px touch target
         and hover fill as ManageHubPage's rows, so the two hub pages read as
         one pattern. Buttons stretch to the row width (`w-full text-left`)
         because a bare button sizes to its content, unlike an anchor row. -->
    <a
      href="/settings"
      class="flex items-center gap-3 min-h-12 px-4 hover:preset-filled-surface-500 transition-colors"
    >
      <Settings class="size-5 shrink-0 opacity-70" aria-hidden="true" />
      <span class="text-sm font-medium">{settings_heading()}</span>
    </a>
    {#if mode === "online"}
      <button
        type="button"
        onclick={() => void logout()}
        class="flex items-center gap-3 min-h-12 px-4 w-full text-left hover:preset-filled-surface-500 transition-colors"
      >
        <LogOut class="size-5 shrink-0 opacity-70" aria-hidden="true" />
        <span class="text-sm font-medium">{account_logout()}</span>
      </button>
      <button
        type="button"
        onclick={() => void switchToOffline()}
        class="flex items-center gap-3 min-h-12 px-4 w-full text-left hover:preset-filled-surface-500 transition-colors"
      >
        <ArrowRightLeft class="size-5 shrink-0 opacity-70" aria-hidden="true" />
        <span class="text-sm font-medium">{account_go_offline()}</span>
      </button>
    {:else}
      <button
        type="button"
        onclick={() => void switchToOnline()}
        class="flex items-center gap-3 min-h-12 px-4 w-full text-left hover:preset-filled-surface-500 transition-colors"
      >
        <ArrowRightLeft class="size-5 shrink-0 opacity-70" aria-hidden="true" />
        <span class="text-sm font-medium">{account_go_online()}</span>
      </button>
    {/if}
  </nav>
</div>
