<script lang="ts">
  // The six-row manage grid: the single source for the row list shared by
  // the /local/manage hub (ManageHubPage renders it as the whole nav) and
  // the offline home's manage section (LocalHomePage stacks it under the
  // chat row). Order and icons mirror the desktop sidebar's manage section
  // one-to-one (links.ts marks that section with `hub`). Icons are imported
  // directly here (not injected via NavIcons) because hub pages already
  // depend on @lucide/svelte directly.
  import {
    Calendar,
    LayoutGrid,
    ListTodo,
    Settings,
    Users,
    Wallet,
  } from "@lucide/svelte";
  import HubRow from "./HubRow.svelte";
  import {
    nav_overview,
    nav_budget,
    nav_agents,
    nav_profiles,
    nav_schedules,
    nav_tasks,
  } from "../paraglide/messages.js";

  const rows = [
    { href: "/local/manage/overview", label: nav_overview, Icon: LayoutGrid },
    { href: "/local/manage/budget", label: nav_budget, Icon: Wallet },
    { href: "/local/manage/agents", label: nav_agents, Icon: Users },
    { href: "/local/manage/profiles", label: nav_profiles, Icon: Settings },
    { href: "/local/manage/schedules", label: nav_schedules, Icon: Calendar },
    { href: "/local/manage/tasks", label: nav_tasks, Icon: ListTodo },
  ];
</script>

<div class="divide-y divide-surface-200-800">
  {#each rows as { href, label, Icon } (href)}
    <HubRow {href} {Icon} label={label()} />
  {/each}
</div>
