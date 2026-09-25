<script lang="ts">
  // The /account landing page: the small-viewport twin of the sidebar
  // footer's AccountMenu dropdown — one full-width row per account entry,
  // reached from the bottom bar's trailing (account) cell. The desktop
  // sidebar keeps its dropdown; this page stays reachable by URL there,
  // mirroring how /local/manage relates to the sidebar's manage section.
  // Rows mix links (settings) and mode actions — HubRow renders each as an
  // anchor or a button. Icons are imported directly here (not injected via
  // NavIcons) because page components already depend on @lucide/svelte
  // directly (PanoramaPage precedent).
  import { LogOut, Settings } from "@lucide/svelte";
  import HubRow from "../../components/HubRow.svelte";
  import { configStore } from "../../lib/config/config.svelte";
  import { shellMode } from "../../lib/shell/port.ts";
  import { logout } from "../../lib/session/account-actions.ts";
  import {
    account_logout,
    account_menu,
    nav_tagmata,
    settings_heading,
  } from "../../paraglide/messages.js";

  // Branch on mode, not on `user` (the AccountMenu invariant): offline must
  // never act on a stale archeion session, so the row set follows the mode.
  const mode = $derived(shellMode());
</script>

<!-- The shell's mobile top row carries this page's heading below md (title prop in RootLayout); this container's padding is plain -- the shell row owns the safe-area inset. -->
<div class="px-2 py-4 md:p-6 max-w-2xl space-y-6">
  <h1 class="text-xl font-semibold hidden md:block">
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
    {/if}
  </nav>
</div>
