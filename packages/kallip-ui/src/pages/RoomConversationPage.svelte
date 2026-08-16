<script lang="ts">
  // One room's conversation: a member-aware transcript rendered from the
  // lesche's payload store + an outbound composer that posts plaintext. Rooms
  // bypass the bilateral projector (no channelsStore / chat_history), so this
  // is a separate page from ChannelChatPage -- the transcript model is
  // multi-member (a sender uuid per line, not a user/assistant role).
  //
  // The page owns the chrome (a header + the loading/error/empty states) and
  // composes the shared <Composer>. The transcript is rendered inline (member-
  // aware: `mine` alignment + a sender label for others); stick-to-tail auto-
  // scroll is the shared `createAutoScroll`. A poll pump refreshes the history
  // + roster on a slow cadence (the room_membership_changed SSE nudge is the
  // fast path; the poll is the dropped-frame backstop).
  import { onMount } from "svelte";
  import { Settings, Users, X } from "@lucide/svelte";
  import Composer from "../components/Composer.svelte";
  import MessageBubble from "../components/MessageBubble.svelte";
  import {
    createAutoScroll,
    createTogglePin,
  } from "../lib/transcript.svelte.ts";
  import { createComposer } from "../lib/composer.svelte.ts";
  import MemberRow from "../components/rooms/MemberRow.svelte";
  import SenderIdentity from "../components/rooms/SenderIdentity.svelte";
  import { roomConversationsStore } from "../lib/session/roomConversations.svelte.ts";
  import { roomsStore } from "../lib/session/rooms.svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import { profileHref } from "../lib/room-message.ts";

  let { roomId }: { roomId: string } = $props();

  const conv = $derived(roomConversationsStore.get(roomId));
  // The registry view for this room (name/description/visibility), for the
  // header title. Null only if the room has not surfaced in the registry yet (a
  // brief resolving window).
  const room = $derived(
    roomsStore.rooms.find((r) => r.room_id === roomId) ?? null,
  );
  const roomLabel = $derived(room?.name || `room ${roomId.slice(0, 8)}`);
  // Display labels keyed by participant id. The wire `Participant` on each
  // message carries the sender's kind + server-resolved handle (the lesche
  // derives it fresh at read time, so it is already correct); it does NOT carry
  // the mutable display label, so the bubble header resolves just the label
  // from the roster. Undefined for a sender not in the roster / before the
  // roster lands (handle alone); re-renders when the roster updates.
  const labelById = $derived(
    new Map((conv?.roster?.members ?? []).map((m) => [m.id, m.label] as const)),
  );

  // Open the conversation (loads the history). Driven from an effect (not a
  // one-shot onMount) gated on the self participant id (needed to flag own
  // messages -- `mine`). Otherwise a refresh races open() ahead of it resolving
  // and bakes wrong `mine` flags into the rendered lines. open() is idempotent
  // and re-opens from an error state, so re-firing once ready recovers it.
  // Refresh the roster alongside for an at-once member count.
  $effect(() => {
    if (!agoraSession.participantId) return;
    void roomConversationsStore.open(roomId);
    void roomConversationsStore.refreshRoster(roomId);
  });

  // Slow poll: the dropped-frame backstop for the room_membership_changed
  // roster nudge + offline-member catch-up. Bounded so a long-idle open tab
  // does not spin. Cleared on unmount.
  onMount(() => {
    const id = setInterval(() => {
      // A terminal error (e.g. this member was removed) stops the poll: the
      // room is gone to this user, and refreshing would only loop the failure.
      if (conv?.status === "error") return;
      void roomConversationsStore.refresh(roomId);
      // Refresh the roster (member count + creator badge) at the same cadence;
      // the room_membership_changed SSE is the faster trigger.
      void roomConversationsStore.refreshRoster(roomId);
    }, 10_000);
    return () => clearInterval(id);
  });

  // Stick to the tail as lines arrive; stop once the user scrolls up to read.
  const scroll = createAutoScroll();
  $effect(() => {
    void conv?.lines.length;
    scroll.stick();
  });

  // One scroll-pin controller for the whole room transcript (a single active
  // ResizeObserver across all bubbles); each <MessageBubble> hands its box +
  // actions elements to it on a raw toggle.
  const togglePin = createTogglePin(() => scroll.viewport);

  const disabled = $derived(conv?.status !== "open");

  // The togglable member side-panel (online dots driven by the room's live
  // online-member set + roster identity).
  let showMembers = $state(false);

  const composer = createComposer({
    send: (text) => roomConversationsStore.send(roomId, text),
    // Busy is not a gate: send renders the optimistic line at once + posts.
    canSubmit: () => conv?.status === "open",
  });

  // The message wire carries the sender's kind + server-resolved stable handle
  // (the lesche derives it fresh at read time via the same resolver the roster
  // uses, so it is correct, not a send-time snapshot). <SenderIdentity>
  // decomposes it at render: `@<username>` for a human,
  // `<id-prefix>@<owner-username>` for an agent. The id-prefix is the
  // unforgeable anchor and the @owner the endorsement, so a mimic cannot pass
  // as a trusted agent. The mutable display name (label) is resolved separately
  // from the roster -- the wire `Participant` carries no label. A Cpu/User icon
  // marks agent/human authorship. Every bubble -- including the caller's own --
  // shows the same display-name + @handle (+ id for agents) header; own bubbles
  // are
  // additionally marked by right-alignment + filled-primary fill (no "you").
</script>

<svelte:head><title>KallipAI · room</title></svelte:head>

<svelte:window
  onkeydown={(e) => showMembers && e.key === "Escape" && (showMembers = false)}
/>

<div class="flex flex-col h-full">
  <header class="px-4 py-2 border-b border-surface-200-800 flex items-center gap-2">
    <div class="flex flex-col min-w-0 flex-1">
      <p class="text-sm font-semibold truncate">{roomLabel}</p>
      <p class="text-xs opacity-50 truncate">
        <span class="opacity-70">Room ID: </span><span class="font-mono"
          >{roomId}</span
        >
      </p>
    </div>
    {#if room?.visibility === "public"}
      <span class="text-xs preset-tonal-surface px-2 py-0.5 rounded-base"
        >public</span
      >
    {/if}
    {#if conv?.roster}
      <span class="text-xs opacity-60">
        {conv.roster.members.length} member{#if conv.roster.members.length !== 1}s{/if}
      </span>
      {#if conv.roster.is_creator}
        <span class="text-xs preset-tonal-surface px-2 py-0.5 rounded-base"
          >creator</span
        >
      {/if}
    {/if}
    <button
      type="button"
      class="size-8 grid place-items-center rounded-base shrink-0 disabled:opacity-60 {showMembers
        ? 'preset-filled-surface-500'
        : 'preset-tonal-surface hover:preset-filled-surface-500'}"
      aria-label="Toggle member list"
      aria-pressed={showMembers}
      disabled={!conv || conv.status === "loading"}
      onclick={() => (showMembers = !showMembers)}
    >
      <Users class="size-4" />
    </button>
    <button
      type="button"
      class="size-8 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-surface-500 shrink-0"
      aria-label="Room settings"
      onclick={() => navigate(`/rooms/${roomId}/settings`)}
    >
      <Settings class="size-4" />
    </button>
  </header>

  {#if !conv || conv.status === "loading"}
    <div class="flex-1 grid place-items-center p-6">
      <p class="text-sm opacity-60">Loading room history…</p>
    </div>
  {:else}
    <div class="flex-1 min-h-0 flex relative">
      <div
        class="flex-1 min-h-0 overflow-auto"
        bind:this={scroll.viewport}
        onscroll={scroll.onScroll}
      >
        <div class="mx-auto w-full max-w-[80rem] p-4 flex flex-col gap-3">
          {#if conv.lines.length === 0}
            <p class="text-sm opacity-60 text-center mt-8">
              Send a message to start the room.
            </p>
          {/if}
          {#each conv.lines as line (line.seq)}
            {@const pending = line.seq < 0}
            {@const href = profileHref(
              line.senderKind,
              line.senderHandle,
              line.senderTagmaId,
            )}
            <div
              class="group flex flex-col {line.mine
                ? 'items-end'
                : 'items-start'}"
            >
              <span
                class="text-xs px-1 mb-0.5 max-w-[80%] min-w-0 flex items-center"
              >
                <SenderIdentity
                  kind={line.senderKind}
                  handle={line.senderHandle}
                  label={labelById.get(line.senderId)}
                  {href}
                />
              </span>
              <MessageBubble
                text={line.text}
                markdown={line.senderKind === "agent"}
                mine={line.mine}
                {pending}
                bare={pending}
                pin={togglePin}
              />
              {#if line.mine && pending && !line.failed}
                <span
                  class="text-xs opacity-50 animate-pulse"
                  aria-label="sending">··</span
                >
              {:else if line.mine && line.failed}
                <button
                  type="button"
                  class="text-xs text-error-500 dark:text-error-400 hover:underline"
                  onclick={() => {
                    // A re-failure marks the new optimistic line failed + renders
                    // its own Retry; the rejection is already surfaced, so swallow
                    // it here to avoid an unhandled rejection.
                    void roomConversationsStore
                      .resend(roomId, line)
                      .catch(() => {});
                  }}>Retry</button
                >
              {/if}
            </div>
          {/each}
          {#if conv.status === "error" && conv.error}
            <p class="text-xs text-error-500 dark:text-error-400 text-center">{conv.error}</p>
          {/if}
        </div>
      </div>
      {#if showMembers}
        <!-- Backdrop: a non-interactive click-catcher that closes the panel on
          mobile (the panel is an overlay on narrow viewports; a flex column on
          wide ones). `role="presentation"` keeps it out of the a11y tree. -->
        <div
          class="absolute inset-0 bg-black/20 lg:hidden"
          role="presentation"
          onclick={() => (showMembers = false)}
        ></div>
        <aside
          class="absolute right-0 top-0 h-full w-80 max-w-[80vw] flex flex-col bg-surface-100-900 border-l border-surface-200-800 shadow-lg lg:static lg:shadow-none z-10"
          aria-label="Room members"
        >
          <div
            class="px-3 py-2 border-b border-surface-200-800 flex items-center justify-between shrink-0"
          >
            <span class="text-sm font-semibold">Members</span>
            <button
              type="button"
              class="size-7 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-surface-500"
              aria-label="Close member list"
              onclick={() => (showMembers = false)}
            >
              <X class="size-4" />
            </button>
          </div>
          <div class="flex-1 min-h-0 overflow-auto p-2 flex flex-col gap-1">
            {#if !conv.roster}
              <p class="text-sm opacity-60 px-1 py-2">Loading…</p>
            {:else}
              {#each conv.roster.members as m (m.id)}
                <MemberRow
                  member={m}
                  selfId={agoraSession.participantId}
                  isCreator={conv.roster.is_creator &&
                    m.id === agoraSession.participantId}
                  online={conv.online.has(m.id)}
                />
              {/each}
            {/if}
          </div>
        </aside>
      {/if}
    </div>
    <Composer
      {composer}
      {disabled}
      pendingCount={0}
      disabledNotice="This room is unavailable."
    />
  {/if}
</div>
