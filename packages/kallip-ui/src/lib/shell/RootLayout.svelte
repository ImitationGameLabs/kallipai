<script lang="ts">
  import { onMount } from "svelte";
  import type { Snippet } from "svelte";
  import AppShell from "../../components/AppShell.svelte";
  import AccountMenu from "../../components/AccountMenu.svelte";
  import TagmaStatusHeader from "../../components/TagmaStatusHeader.svelte";
  import { classifyError } from "../errors.ts";
  import { agoraSession } from "../session/agora.svelte";
  import { channelsStore } from "../session/channels.svelte";
  import { statusCardStore } from "../session/statusCard.svelte.ts";
  import { roomsStore } from "../session/rooms.svelte";
  import { realtimeStore } from "../session/realtime.svelte";
  import { roomConversationsStore } from "../session/roomConversations.svelte";
  import { connectDirect } from "../session/connect.ts";
  import { configStore } from "../config/config.svelte";
  import { modeOf } from "../config/mode.ts";
  import {
    navFor,
    pathMatches,
    tagmaNavIndicator,
    type NavIcons,
  } from "./links.ts";
  import { appGateDecision, isPublicRoute } from "./gate.ts";
  import { navigate } from "./port.ts";
  import { nav_home } from "../../paraglide/messages.js";

  let {
    pathname,
    search,
    icons,
    children,
  }: {
    pathname: string;
    search: string;
    icons: NavIcons;
    children: Snippet;
  } = $props();

  // The mode is the single source of "which product are we in", read from the
  // persisted config's `activeMode` (null config defaults to online).
  const mode = $derived(modeOf(configStore.value));

  // Boot once the config has loaded. The two modes need different boot:
  //   - offline: reconnect the tagma straight away (offline's whole point is
  //     the tagma; on failure surface the error and the connect page will
  //     prompt);
  //   - online: resolve the agora session so the gate reads a settled `user`.
  // onMount (not a reactive $effect) so this runs exactly once, with no
  // `booted` flag and no effect read-of-write hazard.
  onMount(() => {
    // Wire inbound envelopes: demux by recipient -- an envelope for an OPENED
    // room conversation goes to the room store (plaintext render); anything else
    // is the bilateral 1:1 path -> channelsStore. The room store's `get` is the
    // demux (rooms bypass the bilateral projector per the mellow-baking-taco
    // decision). Bound here (the shell, where both singletons are in scope)
    // rather than via a store-to-store import, keeping realtime decoupled from
    // both. Idempotent + safe to run once per mount.
    realtimeStore.setEnvelopeSink((env) => {
      if (roomConversationsStore.get(env.channel_id)) {
        roomConversationsStore.deliverLive(
          env.channel_id,
          env.ciphertext,
          env.sender,
        );
      } else {
        channelsStore.deliver(env);
      }
    });
    // Wire runtime signals (busy/idle presence, turn terminals, errors) into
    // the owning channel's transcript. Same shell-binding discipline.
    realtimeStore.setSignalSink((tagmaId, signal) =>
      channelsStore.deliverSignal(tagmaId, signal),
    );
    // Wire aggregate status snapshots (root state, subagent counts, budget) into
    // the owning channel's `statusSnapshot`, so the chat header reads one
    // uniform source (the direct path drains its own SSE status). Same
    // shell-binding discipline.
    realtimeStore.setStatusSink((tagmaId, snapshot) =>
      channelsStore.deliverStatus(tagmaId, snapshot),
    );
    // Wire the cached-status backfill: a freshly-opened relay channel seeds its
    // `statusSnapshot` from realtime's in-session cache so the header shows at
    // once (otherwise it waits for the next status push). Same shell-binding
    // discipline; keeps channels decoupled from realtime.
    channelsStore.setStatusBackfill((tagmaId) =>
      realtimeStore.statusFor(tagmaId),
    );
    // Wire presence transitions to auto-connect: an offline -> online tagma is
    // opened on demand. Same shell-binding discipline as the envelope sink.
    // NOTE: this is a pre-warm convenience only -- it is no longer load-bearing
    // for sidebar visibility, since enrolled tagmas always show (linked to
    // /chat/t/{tagmaId}, which opens on demand). It just makes the spinner
    // fleeting by opening channels the SSE already knows are online.
    realtimeStore.setPresenceSink((tagmaId, online) => {
      if (!online) return;
      const tagma = agoraSession.tagmata.find(
        (t) => t.tagma_id === tagmaId && t.state === "enrolled",
      );
      if (tagma) void channelsStore.ensureOpen(tagma);
    });
    // Wire room-membership-changed nudges into the room roster refresh: a
    // membership change repaints the member count / creator badge without
    // waiting for the room page's poll. Same shell-binding discipline.
    realtimeStore.setRoomMembershipChangedSink((roomId) => {
      void roomConversationsStore.refreshRoster(roomId);
    });
    // Wire room-member presence deltas into the room's live online-member set:
    // a peer's connect/disconnect mutates the set between roster re-fetches.
    realtimeStore.setRoomMemberPresenceSink((roomId, memberId, online) => {
      roomConversationsStore.applyMemberPresence(roomId, memberId, online);
    });

    void configStore.ready.then(() => {
      const cfg = configStore.value;
      if (cfg?.activeMode === "offline" && cfg.offline) {
        // Surface a boot-reconnect failure on the banner (the same classifier
        // the layout uses for mid-session errors) instead of swallowing it --
        // attachLocal is never reached on failure, so its localError reset does
        // not apply; setting localError directly is correct here.
        connectDirect(cfg.offline)
          .then(({ transport, conversationId }) =>
            channelsStore.attachLocal(transport, conversationId),
          )
          .catch((e) => {
            channelsStore.localError = e;
          });
      } else {
        // Resolve the session; the gate reads the settled `user`. The tagma
        // registry fetch + auto-open are driven by the user_id $effect below
        // (which also re-fires on re-login, unlike this one-shot onMount).
        void agoraSession.whoami();
      }
    });
  });

  // Load the tagma registry + auto-open channels for online tagmas. Keyed on
  // `user?.user_id` (a stable primitive, NOT the `user` object -- whoami
  // reassigns `user` to a fresh object on every fetch): fires at boot, on
  // re-login (a different user_id), and on a mode flip back to online (the
  // cookie survives offline mode, so user_id is stable but `mode` changes).
  // RootLayout.onMount runs once per SPA session, so a re-login would otherwise
  // never re-fetch the registry; this effect is what makes it happen.
  //
  // The post-refresh sweep opens channels for tagmas already showing online at
  // that moment -- it covers the boot ordering where the SSE presence snapshot
  // landed before the registry loaded (the presence sink misses those, since
  // the registry was empty when they fired). Live transitions and snapshots
  // arriving after the sweep are handled by the presence sink. `ensureOpen` is
  // idempotent, so a transition the sink already handled and the sweep both
  // touch is opened exactly once. NOTE: like the presence sink, this is now a
  // pre-warm convenience, not load-bearing for sidebar visibility (enrolled
  // tagmas always show; /chat/t/{tagmaId} opens on demand).
  $effect(() => {
    const uid = agoraSession.user?.user_id;
    if (mode !== "online" || !uid) return;
    void agoraSession.refreshTagmata().then(() => {
      if (!agoraSession.user) return; // logged out mid-flight: gate redirects.
      for (const t of agoraSession.tagmata) {
        if (t.state === "enrolled" && realtimeStore.has(t.tagma_id)) {
          void channelsStore.ensureOpen(t);
        }
      }
    });
  });

  // Load the rooms registry + invite inbox for the signed-in online user. A
  // sibling effect to the tagmata one (rooms are a separate concern; the tagma
  // effect ends in a channel auto-open sweep that is unrelated). Same keying
  // discipline: `user?.user_id` (a stable primitive), not the `user` object.
  $effect(() => {
    const uid = agoraSession.user?.user_id;
    if (mode !== "online" || !uid) return;
    void roomsStore.refresh();
  });

  // Load the signed-in user's passkeys (devices). Gated on `!passkeysLoaded` so
  // it cooperates with SettingsPage's own passkey-load effect (whichever fires
  // first loads; the other no-ops) -- two triggers with the SAME guard, not a
  // maintenance trap. Keyed on user_id; reset() clears passkeysLoaded on logout.
  $effect(() => {
    const uid = agoraSession.user?.user_id;
    if (mode !== "online" || !uid || agoraSession.passkeysLoaded) return;
    void agoraSession.refreshPasskeys();
  });

  // Run the realtime SSE feed (presence + envelope delivery) while signed-in in
  // online mode; tear it down otherwise. Keyed on `user?.user_id` (a stable
  // primitive), NOT the `user` object: whoami() reassigns `user` to a fresh
  // object on every fetch, so keying on the object would cycle the feed on each
  // re-fetch. user_id still changes on login-as-different-user / logout, so the
  // cleanup fires exactly when it should.
  $effect(() => {
    const uid = agoraSession.user?.user_id;
    if (mode === "online" && uid) {
      realtimeStore.start();
      return () => {
        realtimeStore.stop();
      };
    }
  });

  const decision = $derived(
    appGateDecision({
      loaded: configStore.loaded,
      mode,
      user: agoraSession.user,
      authError: agoraSession.authError,
      connected: channelsStore.localConnected,
      pathname,
      search,
    }),
  );

  // Act on a redirect decision. replaceState so the guarded URL never enters
  // history (Back returns to the pre-app referrer, not a redirect loop).
  $effect(() => {
    if (decision.kind === "redirect") {
      void navigate(decision.url, { replaceState: true });
    }
  });

  // The online sidebar lists EVERY enrolled tagma (not just open channels):
  // the indicator reflects the channel transport state, and the entry links to
  // the tagma-keyed route /chat/t/{tagmaId} which opens the channel on demand.
  // Channel-transport-only (no realtime/presence) so the dot stays honest when
  // the realtime SSE is down; peer presence still lives on the /tagmata
  // dashboard.
  const tagmaNav = $derived(
    agoraSession.enrolledCards.map((c) => ({
      tagmaId: c.tagmaId,
      label: c.label,
      indicator: tagmaNavIndicator(
        channelsStore.getTagmaChannelState(c.tagmaId),
      ),
    })),
  );

  const links = $derived(
    navFor({
      mode,
      icons,
      tagmata: tagmaNav,
      rooms: roomsStore.rooms.map((r) => ({
        roomId: r.room_id,
        label: r.name || `room ${r.room_id.slice(0, 8)}`,
      })),
    }),
  );

  // Segment-boundary match so sibling /chat/{id} entries do not cross-highlight.
  function isActive(href: string): boolean {
    return pathMatches(pathname, href);
  }

  // Offline error: the local conversation's transport-level error (mid-session
  // tagma failure) or localError (a boot-reconnect / mode-switch failure that
  // landed before a local conversation existed). The banner classifies it; the
  // full error is mirrored to the console.
  const offlineError = $derived(
    channelsStore.local?.error ?? channelsStore.localError,
  );
  const errorView = $derived(offlineError ? classifyError(offlineError) : null);
  $effect(() => {
    if (offlineError) console.error(offlineError);
  });
  // Offline content pages (any /local/* below the home itself) swap the
  // small-screen bottom bar for the back row above; the home and every
  // online route keep the bar. Desktop is unaffected (the row is md:hidden
  // and the sidebar always renders).
  const back = $derived(
    mode === "offline" &&
    pathname.startsWith("/local/") &&
    pathname !== "/local"
      ? { href: "/local", label: nav_home() }
      : null);
</script>

<!-- Sidebar footer entry; see AccountMenu for behavior. -->
{#snippet statusSnippet()}
  <AccountMenu />
{/snippet}

<!-- Mobile top row for offline /local/chat: the page's status header lifts
     into the shell so it rides beside the back chevron and above the
     banner; only handed over when a back row exists at all. -->
{#snippet topRowSnippet()}
  {#if mode === "offline" && pathname === "/local/chat"}
    <TagmaStatusHeader
      status={channelsStore.get("local")?.statusSnapshot}
      agentRows={{
        rootRow: statusCardStore.rootRow,
        subRows: statusCardStore.subRows,
      }}
      sideLayout={false}
    />
  {/if}
{/snippet}

{#if decision.kind === "render" && isPublicRoute(pathname)}
  {@render children()}
{:else if decision.kind === "render"}
  <AppShell
    {links}
    {isActive}
    {back}
    topRow={back ? topRowSnippet : undefined}
    error={errorView}
    status={statusSnippet}
  >
    {@render children()}
  </AppShell>
{:else}
  <!-- skeleton: config still loading (mode unknown) or whoami in flight (online,
       no error yet). An auth failure routes the user to /login (see
       appGateDecision), so this branch is only the brief resolving window.
       Never a protected AppShell, so no gated content flashes. -->
  <div class="p-4"><p class="opacity-60">Loading…</p></div>
{/if}
