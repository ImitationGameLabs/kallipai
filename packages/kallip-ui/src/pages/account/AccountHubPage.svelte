<script lang="ts">
  // The /account landing page: the small-viewport twin of the sidebar
  // footer's AccountMenu dropdown — one full-width row per account entry,
  // reached from the bottom bar's trailing (account) cell. The desktop
  // sidebar keeps its dropdown; this page stays reachable by URL there,
  // mirroring how /local/manage relates to the sidebar's manage section.
  // Rows mix links (settings) and mode actions — HubRow renders each as an
  // anchor or a button. Icons are imported directly here (not injected via
  // NavIcons) because page components already depend on @lucide/svelte
  // directly (ManageHubPage precedent).
  import { ArrowRightLeft, LogOut, Settings } from "@lucide/svelte";
  import HubRow from "../../components/HubRow.svelte";
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

<!-- pt: calc keeps the browser value (1rem) when the inset is 0 and adds the system-bar height under edge-to-edge; these hub pages have no shell top row of their own. -->
<div
  class="px-2 pt-[calc(1rem+env(safe-area-inset-top))] pb-4 md:p-6 max-w-2xl space-y-6"
>
  <h1 class="text-xl font-semibold text-center md:text-left">
    {account_menu()}
  </h1>

  <!-- Row order mirrors the dropdown: settings, then the mode actions. -->
  <nav
    class="card preset-tonal-surface divide-y divide-surface-200-800"
    aria-label={account_menu()}
  >
    <HubRow href="/settings" Icon={Settings} label={settings_heading()} />
    {#if mode === "online"}
      <HubRow
        onclick={() => void logout()}
        Icon={LogOut}
        label={account_logout()}
      />
      <HubRow
        onclick={() => void switchToOffline()}
        Icon={ArrowRightLeft}
        label={account_go_offline()}
      />
    {:else}
      <HubRow
        onclick={() => void switchToOnline()}
        Icon={ArrowRightLeft}
        label={account_go_online()}
      />
    {/if}
  </nav>
</div>
