<script lang="ts">
  // The /local/manage landing page: one full-width row per management page.
  // This is the small-screen destination of the bottom bar's single "manage"
  // cell (links.ts marks that section with `hub`); the desktop sidebar keeps
  // its five direct links and simply never renders this page's entry, so the
  // row list below mirrors the sidebar section's order and icons one-to-one.
  // Icons are imported directly here (not injected via NavIcons) because page
  // components in this folder already depend on @lucide/svelte directly.
  import {
    Calendar,
    LayoutGrid,
    Settings,
    Users,
    Wallet,
  } from "@lucide/svelte";
  import HubRow from "../../components/HubRow.svelte";
  import {
    nav_manage,
    nav_overview,
    nav_budget,
    nav_agents,
    nav_profiles,
    nav_schedules,
  } from "../../paraglide/messages.js";

  const rows = [
    { href: "/local/manage/overview", label: nav_overview, Icon: LayoutGrid },
    { href: "/local/manage/budget", label: nav_budget, Icon: Wallet },
    { href: "/local/manage/agents", label: nav_agents, Icon: Users },
    { href: "/local/manage/profiles", label: nav_profiles, Icon: Settings },
    { href: "/local/manage/schedules", label: nav_schedules, Icon: Calendar },
  ];
</script>

<div class="px-2 py-4 md:p-6 max-w-2xl space-y-6">
  <h1 class="text-xl font-semibold">{nav_manage()}</h1>

  <nav
    class="card preset-tonal-surface divide-y divide-surface-200-800"
    aria-label={nav_manage()}
  >
    {#each rows as { href, label, Icon } (href)}
      <HubRow {href} {Icon} label={label()} />
    {/each}
  </nav>
</div>
