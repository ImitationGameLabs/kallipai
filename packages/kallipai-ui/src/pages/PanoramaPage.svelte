<script lang="ts">
  // The panorama: the product home ('/'), desktop-only. A 12-column bento --
  // the sessions region (8 cols) and the tagmata column (4 cols) project the
  // same stores the chats hub reads; the second bento row carries the files
  // entry card; the extension slot stays reserved and renders
  // nothing (no placeholder card, no copy). Small screens never see
  // this page: the root route's load redirects to /chats before mount, and
  // the listener below covers a desktop->mobile crossing afterwards. Pure
  // store projection: the page owns no fetches of its own.
  import { Cpu, Users } from "@lucide/svelte";
  import HubRow from "../components/HubRow.svelte";
  import { desktopQuery } from "../lib/shell/breakpoint.ts";
  import { navigate } from "../lib/shell/port.ts";
  import { archeionSession } from "../lib/session/archeion.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { realtimeStore } from "../lib/session/realtime.svelte.ts";
  import { roomsStore } from "../lib/session/rooms.svelte";
  import { directSessionsStore } from "../lib/session/directSessions.svelte";
  import { filesPath, tagmaChatPath } from "../lib/shell/routes.ts";
  import { tagmaNavIndicator } from "../lib/shell/links.ts";
  import {
    unreadStore,
    roomKey,
    tagmaKey,
  } from "../lib/session/unread.svelte.ts";
  import {
    nav_chats,
    nav_chats_empty,
    nav_home,
    files_heading,
    files_panorama_blurb,
    files_panorama_open,
    nav_manage,
    nav_tagmata,
    panorama_view_all,
    room_label_fallback,
    tagma_profile_unnamed,
  } from "../paraglide/messages.js";
  import {
    panoramaSessionRows,
    type PanoramaSessionRow,
  } from "./panoramaProjection.ts";

  // Visible session rows per the aggregation summary; the rest live on the
  // unbounded /chats list this region links to.
  const SESSIONS_CAP = 6;

  // Breakpoint crossing: being mounted stopped meaning "desktop" the moment
  // the viewport shrank below the fork -- hand back to the small-screen home.
  const mdQuery = matchMedia(desktopQuery);
  $effect(() => {
    const onChange = (event: MediaQueryListEvent) => {
      if (!event.matches) navigate("/chats");
    };
    mdQuery.addEventListener("change", onChange);
    return () => mdQuery.removeEventListener("change", onChange);
  });

  const sessionRows = $derived(
    panoramaSessionRows(
      [
        ...archeionSession.enrolledCards.map((c) => ({
          href: tagmaChatPath(c.tagmaId),
          label: c.label ?? tagma_profile_unnamed(),
          kind: "tagma" as const,
          badge: unreadStore.countOf(tagmaKey(c.tagmaId)),
        })),
        ...roomsStore.rooms.map((r) => ({
          href: `/rooms/${r.room_id}`,
          label: r.name || room_label_fallback({ id: r.room_id.slice(0, 8) }),
          kind: "room" as const,
          badge: unreadStore.countOf(roomKey(r.room_id)),
        })),
        ...directSessionsStore.list().map((s) => ({
          href: `/tagma/${s.tagmaId}/direct/${s.peerId}`,
          label: directSessionsStore.peerLabel(s.peerId, s.peerHandle),
          kind: "direct" as const,
        })),
      ],
      SESSIONS_CAP,
    ),
  );

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
</script>

<svelte:head><title>{nav_home()}</title></svelte:head>

<div class="p-6 max-w-6xl mx-auto grid grid-cols-12 gap-6 items-start">
  <!-- Bento template: sessions 8 + tagmata 4 fill the first row; the second
       row's files slot (8) carries the entry card; extension (4) stays
       reserved, so the column budget stays honest. -->
  <section class="col-span-8 space-y-3" aria-label={nav_chats()}>
    <div class="flex items-baseline justify-between gap-4">
      <h2 class="text-sm font-semibold uppercase tracking-wide opacity-60">
        {nav_chats()}
      </h2>
      <a href="/chats" class="text-sm link">{panorama_view_all()}</a>
    </div>
    {#if sessionRows.length > 0}
      <div class="card preset-tonal-surface divide-y divide-surface-200-800">
        {#each sessionRows as row (row.href)}
          <HubRow
            href={row.href}
            Icon={row.kind === "room" ? Users : undefined}
            label={row.label}
            badge={row.badge}
          />
        {/each}
      </div>
    {:else}
      <div class="card preset-tonal-surface px-4 py-4 text-sm opacity-70">
        {nav_chats_empty()}
      </div>
    {/if}
  </section>

  <section class="col-span-4 space-y-3" aria-label={nav_tagmata()}>
    <h2 class="text-sm font-semibold uppercase tracking-wide opacity-60">
      {nav_tagmata()}
    </h2>
    {#if tagmaRows.length > 0}
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
    {/if}
    <div class="card preset-tonal-surface">
      <HubRow href="/tagmata" Icon={Cpu} label={nav_manage()} />
    </div>
  </section>
  <!-- The files region: a static entry card -- title, one
       line of copy, one link. Zero data dependencies by design -- the
       region stays fully decoupled from live stores. -->
  <section class="col-span-8 space-y-3" aria-label={files_heading()}>
    <h2 class="text-sm font-semibold uppercase tracking-wide opacity-60">
      {files_heading()}
    </h2>
    <div class="card preset-tonal-surface p-6 flex flex-col gap-3">
      <p class="text-sm opacity-80">{files_panorama_blurb()}</p>
      <a href={filesPath()} class="btn preset-filled-primary-500 self-start">
        {files_panorama_open()}
      </a>
    </div>
  </section>
</div>
