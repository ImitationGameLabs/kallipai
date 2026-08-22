<script lang="ts">
  // The /local landing page: the offline home. Small screens arrive from the
  // bottom bar's home cell; the page gathers the mode's two surfaces at once —
  // a full-width chat CTA (with the latest local-conversation line as a
  // one-line preview when a transcript exists) above the manage grid, which
  // mirrors the desktop sidebar's direct links (ManageHubPage stays as the
  // /local/manage hub shape). Icons are imported directly here (ManageHubPage
  // precedent: page components already depend on @lucide/svelte).
  import {
    Calendar,
    LayoutGrid,
    MessageSquare,
    Settings,
    Users,
    Wallet,
  } from "@lucide/svelte";
  import HubRow from "../components/HubRow.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import {
    nav_home,
    nav_chat,
    nav_manage,
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

  // One-line preview under the CTA: the newest local transcript line. The
  // store seeds the transcript from cache, so this is usually instant.
  const preview = $derived(channelsStore.local?.transcript.lines.at(-1)?.text);
</script>

<div class="px-2 py-4 md:p-6 max-w-2xl space-y-6">
  <h1 class="text-xl font-semibold">{nav_home()}</h1>

  <div class="space-y-2">
    <a
      href="/local/chat"
      class="btn preset-filled-primary-500 w-full h-14 text-lg grid place-items-center"
    >
      <span class="flex items-center gap-2">
        <MessageSquare class="size-5 shrink-0" aria-hidden="true" />
        {nav_chat()}
      </span>
    </a>
    {#if preview}
      <p class="text-sm opacity-60 truncate px-2">{preview}</p>
    {/if}
  </div>

  <nav
    class="card preset-tonal-surface divide-y divide-surface-200-800"
    aria-label={nav_manage()}
  >
    {#each rows as { href, label, Icon } (href)}
      <HubRow {href} {Icon} label={label()} />
    {/each}
  </nav>
</div>
