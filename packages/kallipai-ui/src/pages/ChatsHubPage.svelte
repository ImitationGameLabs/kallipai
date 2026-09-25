<script lang="ts">
  // The /chats hub: the small-screen landing for conversations -- every
  // enrolled tagma chat (indicator dots) followed by the caller's rooms, in
  // the same shape links.ts derives for the sidebar. Pure conversation rows:
  // no manage chip (rooms management lives on the combined /tagmata page).
  // Reached from the bottom bar's Chats cell and the panorama's view-all
  // link; no `back` prop, so the bar stays visible. Rows read the stores
  // the same way RootLayout does; the page owns no fetches of its own.
  import { Cpu, Users } from "@lucide/svelte";
  import HubRow from "../components/HubRow.svelte";
  import { archeionSession } from "../lib/session/archeion.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { realtimeStore } from "../lib/session/realtime.svelte.ts";
  import { roomsStore } from "../lib/session/rooms.svelte";
  import { tagmaChatPath } from "../lib/shell/routes.ts";
  import {
    unreadStore,
    roomKey,
    tagmaKey,
  } from "../lib/session/unread.svelte.ts";
  import { tagmaNavIndicator } from "../lib/shell/links.ts";
  import { directSessionsStore } from "../lib/session/directSessions.svelte";
  import {
    nav_chats,
    nav_chats_empty,
    nav_manage,
    nav_rooms,
    nav_tagmata,
    tagma_profile_unnamed,
    room_label_fallback,
    chat_direct_section,
  } from "../paraglide/messages.js";

  const tagmaRows = $derived(
    archeionSession.enrolledCards.map((c) => ({
      href: tagmaChatPath(c.tagmaId),
      label: c.label ?? tagma_profile_unnamed(),
      indicator: tagmaNavIndicator(
        channelsStore.getTagmaChannelState(c.tagmaId),
        realtimeStore.resolved && !realtimeStore.has(c.tagmaId),
      ),
      badge: unreadStore.countOf(tagmaKey(c.tagmaId)),
    })),
  );
  const directRows = $derived(
    directSessionsStore.list().map((s) => ({
      href: `/tagma/${s.tagmaId}/direct/${s.peerId}`,
      label: directSessionsStore.peerLabel(s.peerId, s.peerHandle),
    })),
  );
  const roomRows = $derived(
    roomsStore.rooms.map((r) => ({
      href: `/rooms/${r.room_id}`,
      label: r.name || room_label_fallback({ id: r.room_id.slice(0, 8) }),
      badge: unreadStore.countOf(roomKey(r.room_id)),
    })),
  );
</script>

<!-- pt: calc keeps the browser value (1rem) when the inset is 0 and adds the system-bar height under edge-to-edge; these hub pages have no shell top row of their own. -->
<div
  class="px-2 pt-[calc(1rem+env(safe-area-inset-top))] pb-4 md:p-6 max-w-2xl space-y-6"
>
  <h1 class="text-xl font-semibold text-center md:text-left">
    {nav_chats()}
  </h1>

  {#if tagmaRows.length > 0}
    <section class="space-y-3" aria-label={nav_tagmata()}>
      <h2 class="text-sm font-semibold uppercase tracking-wide opacity-60">
        {nav_tagmata()}
      </h2>
      <div class="card preset-tonal-surface divide-y divide-surface-200-800">
        {#each tagmaRows as row (row.href)}
          <HubRow
            href={row.href}
            label={row.label}
            indicator={row.indicator}
            badge={row.badge}
          />
        {/each}
      </div>
    </section>
  {/if}

  {#if roomRows.length > 0}
    <section class="space-y-3" aria-label={nav_rooms()}>
      <h2 class="text-sm font-semibold uppercase tracking-wide opacity-60">
        {nav_rooms()}
      </h2>
      <div class="card preset-tonal-surface divide-y divide-surface-200-800">
        {#each roomRows as row (row.href)}
          <HubRow
            href={row.href}
            Icon={Users}
            label={row.label}
            badge={row.badge}
          />
        {/each}
      </div>
    </section>
  {/if}
  {#if directRows.length > 0}
    <section class="space-y-3" aria-label={chat_direct_section()}>
      <h2 class="text-sm font-semibold uppercase tracking-wide opacity-60">
        {chat_direct_section()}
      </h2>
      <div class="card preset-tonal-surface divide-y divide-surface-200-800">
        {#each directRows as row (row.href)}
          <HubRow href={row.href} label={row.label} />
        {/each}
      </div>
    </section>
  {/if}

  {#if tagmaRows.length === 0 && roomRows.length === 0 && directRows.length === 0}
    <!-- First-run empty state: explain, then two CTAs (enroll a tagma on the
         combined manage page, or find rooms). -->
    <div class="card preset-tonal-surface">
      <p class="text-sm opacity-70 px-4 py-4">{nav_chats_empty()}</p>
      <div class="divide-y divide-surface-200-800">
        <HubRow href="/tagmata" Icon={Cpu} label={nav_manage()} />
        <HubRow href="/rooms" Icon={Users} label={nav_rooms()} />
      </div>
    </div>
  {/if}
</div>
