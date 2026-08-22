<script lang="ts">
  // The /local landing page: the offline home. Small screens arrive from the
  // bottom bar's home cell. One card gathers the mode's surfaces: the chat
  // entry first, then a stronger divider, then the manage grid (mirrors the
  // desktop sidebar's direct links; ManageHubPage stays as the /local/manage
  // hub shape). Icons are imported directly here (ManageHubPage precedent:
  // page components already depend on @lucide/svelte).
  import {
    Calendar,
    LayoutGrid,
    MessageSquare,
    Settings,
    Users,
    Wallet,
  } from "@lucide/svelte";
  import HubRow from "../components/HubRow.svelte";
  import {
    nav_home,
    nav_chat,
    nav_overview,
    nav_budget,
    nav_agents,
    nav_profiles,
    nav_schedules,
  } from "../paraglide/messages.js";

  const rows = [
    { href: "/local/manage/overview", label: nav_overview, Icon: LayoutGrid },
    { href: "/local/manage/budget", label: nav_budget, Icon: Wallet },
    { href: "/local/manage/agents", label: nav_agents, Icon: Users },
    { href: "/local/manage/profiles", label: nav_profiles, Icon: Settings },
    { href: "/local/manage/schedules", label: nav_schedules, Icon: Calendar },
  ];
</script>

<!-- pt: calc keeps the browser value (1rem) when the inset is 0 and adds the system-bar height under edge-to-edge; these hub pages have no shell top row of their own. -->
<div
  class="px-2 pt-[calc(1rem+env(safe-area-inset-top))] pb-4 md:p-6 max-w-2xl"
>
  <nav class="card preset-tonal-surface" aria-label={nav_home()}>
    <HubRow href="/local/chat" Icon={MessageSquare} label={nav_chat()} />
    <div class="border-t-2 border-surface-300-700"></div>
    <div class="divide-y divide-surface-200-800">
      {#each rows as { href, label, Icon } (href)}
        <HubRow {href} {Icon} label={label()} />
      {/each}
    </div>
  </nav>
</div>
