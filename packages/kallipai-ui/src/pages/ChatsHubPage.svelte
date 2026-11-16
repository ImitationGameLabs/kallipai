<script lang="ts">
  // The /chats hub: the small-screen landing for conversations, ordered by
  // when each one last moved. Two folds, both open on arrival: the caller's
  // rooms, then the tagma group -- which also holds the agent-to-agent
  // sessions under their own subheading, so the two kinds never blur into one
  // list. The tagma rows keep the label, status dot and unread badge they had;
  // only the container folds, and the fold state lives in this component, so
  // reopening the page starts from the same place. Reached from the bottom
  // bar's Chats cell and the panorama's view-all link; no `back` prop, so the
  // bar stays visible. Rows read the stores the same way RootLayout does; the
  // page owns no fetches of its own.
  import { ChevronDown, Cpu, Users } from "@lucide/svelte";
  import { Accordion } from "@skeletonlabs/skeleton-svelte";
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
  import {
    directSessionsStore,
    pairKey,
  } from "../lib/session/directSessions.svelte";
  import { recentConversations } from "../lib/session/recent.ts";
  import { recentStore } from "../lib/session/recent.svelte.ts";
  import { recentStamp } from "../lib/shell/navTime.svelte.ts";
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

  // All three groups share the sidebar's order: the ledger decides, rows it
  // has never seen keep their registration order after the stamped ones, and
  // nothing is capped here -- this page is the manager.
  const ledger = $derived(recentStore.ledger());
  // Stamps are derived from the ledger, so they refresh whenever it moves and
  // read the clock at that moment. A page left open with no traffic keeps the
  // stamps it derived until the next entry arrives.
  const stamps = $derived.by(() => {
    const now = Date.now();
    const out = new Map<string, string>();
    for (const [key, at] of Object.entries(ledger)) {
      out.set(key, recentStamp(at, now));
    }
    return out;
  });
  const stamp = (key: string): string | undefined => stamps.get(key);

  const tagmaRows = $derived.by(() => {
    const rows = archeionSession.enrolledCards.map((c) => {
      const key = tagmaKey(c.tagmaId);
      const unread = unreadStore.countOf(key);
      return {
        key,
        unread,
        href: tagmaChatPath(c.tagmaId),
        label: c.label ?? tagma_profile_unnamed(),
        indicator: tagmaNavIndicator(
          channelsStore.getTagmaChannelState(c.tagmaId),
          realtimeStore.resolved && !realtimeStore.has(c.tagmaId),
        ),
        badge: unread,
      };
    });
    return recentConversations(rows, ledger).map((r) => ({
      ...r,
      time: stamp(r.key),
    }));
  });

  const roomRows = $derived.by(() => {
    const rows = roomsStore.rooms.map((r) => {
      const key = roomKey(r.room_id);
      const unread = unreadStore.countOf(key);
      return {
        key,
        unread,
        href: `/rooms/${r.room_id}`,
        label: r.name || room_label_fallback({ id: r.room_id.slice(0, 8) }),
        badge: unread,
      };
    });
    return recentConversations(rows, ledger).map((r) => ({
      ...r,
      time: stamp(r.key),
    }));
  });

  // The agent-to-agent group: a row carries a stamp once the reader has
  // opened it (DirectSessionPage is the only writer for this key), so until
  // then it sorts by registration after the stamped rows.
  const directRows = $derived.by(() => {
    const rows = directSessionsStore.list().map((s) => ({
      key: pairKey(s.tagmaId, s.peerId),
      href: `/tagma/${s.tagmaId}/direct/${s.peerId}`,
      label: directSessionsStore.peerLabel(s.peerId, s.peerHandle),
    }));
    return recentConversations(rows, ledger).map((r) => ({
      ...r,
      time: stamp(r.key),
    }));
  });

  // Both folds start open; the state stays in this component, so reopening
  // the page starts from the same place and nothing is persisted.
  let openSections = $state(["rooms", "tagmata"]);
</script>

<!-- pt: calc keeps the browser value (1rem) when the inset is 0 and adds the system-bar height under edge-to-edge; these hub pages have no shell top row of their own. -->
<div
  class="px-2 pt-[calc(1rem+env(safe-area-inset-top))] pb-4 md:p-6 max-w-2xl space-y-6"
>
  <h1 class="text-xl font-semibold text-center md:text-left">
    {nav_chats()}
  </h1>

  <!-- Two folds, both open on arrival. The tagma group keeps its rows,
       title and status dots; only its container folds, and the
       agent-to-agent group sits inside it under its own subheading so the
       two kinds never blur into one list. -->
  <Accordion
    multiple
    collapsible
    value={openSections}
    onValueChange={(details) => (openSections = details.value as string[])}
    class="space-y-4"
  >
    {#if roomRows.length > 0}
      <Accordion.Item value="rooms" class="card preset-tonal-surface">
        <!-- APG shape: the heading wraps the button, not the other way round.
             The trigger renders a <button>, which cannot hold an <h2>. -->
        <h2 class="m-0">
          <Accordion.ItemTrigger
            class="flex w-full items-center gap-2 px-4 py-3 text-left"
          >
            <span
              class="text-sm font-semibold uppercase tracking-wide opacity-80"
            >
              {nav_rooms()}
            </span>
            <span class="text-xs tabular-nums opacity-80">
              {roomRows.length}
            </span>
            <!-- The indicator renders an empty div unless it is given
                 something: zag adds state attributes only, never a glyph. -->
            <Accordion.ItemIndicator
              class="ml-auto transition-transform data-[state=open]:rotate-180"
            >
              <ChevronDown class="size-4" aria-hidden="true" />
            </Accordion.ItemIndicator>
          </Accordion.ItemTrigger>
        </h2>
        <!-- mx-4 px-0: the fold content part's own padding turned into a
             margin, so the border starts where the rows do instead of
             running to the card's edge. -->
        <Accordion.ItemContent
          class="mx-4 px-0 divide-y divide-surface-200-800 border-t border-surface-200-800"
        >
          {#each roomRows as row (row.href)}
            <HubRow
              href={row.href}
              Icon={Users}
              label={row.label}
              badge={row.badge}
              time={row.time}
            />
          {/each}
        </Accordion.ItemContent>
      </Accordion.Item>
    {/if}

    {#if tagmaRows.length > 0 || directRows.length > 0}
      <Accordion.Item value="tagmata" class="card preset-tonal-surface">
        <h2 class="m-0">
          <Accordion.ItemTrigger
            class="flex w-full items-center gap-2 px-4 py-3 text-left"
          >
            <span
              class="text-sm font-semibold uppercase tracking-wide opacity-80"
            >
              {nav_tagmata()}
            </span>
            <span class="text-xs tabular-nums opacity-80">
              {tagmaRows.length + directRows.length}
            </span>
            <!-- The indicator renders an empty div unless it is given
                 something: zag adds state attributes only, never a glyph. -->
            <Accordion.ItemIndicator
              class="ml-auto transition-transform data-[state=open]:rotate-180"
            >
              <ChevronDown class="size-4" aria-hidden="true" />
            </Accordion.ItemIndicator>
          </Accordion.ItemTrigger>
        </h2>
        <!-- Same inset as the rooms fold: the border runs where the rows do. -->
        <Accordion.ItemContent
          class="mx-4 px-0 border-t border-surface-200-800"
        >
          <div class="divide-y divide-surface-200-800">
            {#each tagmaRows as row (row.href)}
              <HubRow
                href={row.href}
                label={row.label}
                indicator={row.indicator}
                badge={row.badge}
                time={row.time}
              />
            {/each}
          </div>
          {#if directRows.length > 0}
            <div class="border-t border-surface-200-800">
              <h3
                class="px-4 pt-3 pb-1 text-xs font-semibold uppercase tracking-wide opacity-50"
              >
                {chat_direct_section()}
              </h3>
              <div class="divide-y divide-surface-200-800">
                {#each directRows as row (row.href)}
                  <HubRow href={row.href} label={row.label} time={row.time} />
                {/each}
              </div>
            </div>
          {/if}
        </Accordion.ItemContent>
      </Accordion.Item>
    {/if}
  </Accordion>

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
