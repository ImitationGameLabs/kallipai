<script lang="ts">
  // One room's conversation: a member-aware transcript rendered from the
  // lesche's payload store + an outbound composer that posts plaintext. Rooms
  // bypass the bilateral projector (no channelsStore / chat_history), so this
  // is a separate page from ChannelChatPage -- the transcript model is
  // multi-member (a sender uuid per line, not a user/assistant role).
  //
  // The page composes the shared chrome (PageHeader top bar + its own
  // loading/error/empty states) and the shared <Composer>. The transcript
  // renders inline (member-
  // aware: `mine` alignment + a sender label for others); stick-to-tail auto-
  // scroll is the shared `createAutoScroll`. A poll pump refreshes the history
  // + roster on a slow cadence (the room_membership_changed SSE nudge is the
  // fast path; the poll is the dropped-frame backstop).
  import { onMount, untrack } from "svelte";
  import { Settings, Users, X } from "@lucide/svelte";
  import Composer from "../components/Composer.svelte";
  import MessageBubble from "../components/MessageBubble.svelte";
  import PageHeader from "../components/PageHeader.svelte";
  import ScrollToBottomButton from "../components/ScrollToBottomButton.svelte";
  import {
    createAutoScroll,
    createTogglePin,
  } from "../lib/transcript.svelte.ts";
  import type { ArmedQuote } from "../lib/quote.ts";
  import {
    armedQuoteSeq,
    previewOf,
    roomQuoteBlocksByLine,
  } from "../lib/quote.ts";
  import { createReveal, revealMark } from "../lib/reveal.svelte.ts";
  import { ROW_TIME } from "../lib/time/stamp.ts";
  import { createComposer } from "../lib/composer.svelte.ts";
  import { createMentionPicker } from "../lib/mentionPicker.svelte.ts";
  import { mentionOptionId } from "../lib/mentions.ts";
  import { bindDraft } from "../lib/session/drafts.svelte.ts";
  import { roomDraftKey } from "../lib/session/drafts.ts";
  import {
    mentionChipsOf,
    type RoomLine,
    roomRowAnnouncement,
    roomLinePending,
    roomMessageLabel,
    roomRowAnchorId,
  } from "../lib/room-message.ts";
  import MemberRow from "../components/rooms/MemberRow.svelte";
  import MentionPicker from "../components/rooms/MentionPicker.svelte";
  import MentionChips from "../components/MentionChips.svelte";
  import MessageRow from "../components/MessageRow.svelte";
  import QuoteButton from "../components/QuoteButton.svelte";
  import QuoteBar from "../components/QuoteBar.svelte";
  import SenderIdentity from "../components/rooms/SenderIdentity.svelte";
  import { roomConversationsStore } from "../lib/session/roomConversations.svelte.ts";
  import { roomsStore } from "../lib/session/rooms.svelte";
  import { archeionSession } from "../lib/session/archeion.svelte";
  import { unreadStore, roomKey } from "../lib/session/unread.svelte.ts";
  import { navigate } from "../lib/shell/port.ts";
  import { profileHref } from "../lib/room-message.ts";
  import { startVisibleInterval } from "../lib/visibleInterval.ts";
  import { TONAL_ICON_SURF } from "../lib/classes.ts";
  import { formatStamp } from "../lib/time/stamp.svelte.ts";
  import {
    common_loading,
    common_retry,
    room_title,
    room_label_fallback,
    room_member_one,
    room_member_other,
    room_id_label,
    room_public_badge,
    room_creator_badge,
    room_toggle_members_aria,
    room_settings_aria,
    room_loading_history,
    room_empty,
    room_sending_aria,
    room_members_aria,
    room_members,
    room_close_members_aria,
    room_unavailable,
    nav_rooms,
  } from "../paraglide/messages.js";

  let { roomId }: { roomId: string } = $props();

  const conv = $derived(roomConversationsStore.get(roomId));
  // The registry view for this room (name/description/visibility), for the
  // header title. Null only if the room has not surfaced in the registry yet (a
  // brief resolving window).
  const room = $derived(
    roomsStore.rooms.find((r) => r.room_id === roomId) ?? null,
  );
  const roomLabel = $derived(
    room?.name || room_label_fallback({ id: roomId.slice(0, 8) }),
  );

  // The quote block each line draws, resolved once per render over the window
  // already on screen. The room transcript keeps every line it has loaded (a
  // history page is capped at 200 rows, but the store appends pages and live
  // frames without trimming), so a lookup per row would be quadratic in it.
  const quotes = $derived(roomQuoteBlocksByLine(conv?.lines ?? []));

  // The roster keyed by participant id: one pass serves both the sender label
  // the bubble header draws and the members a line mentions. The wire
  // `Participant` on each message carries the sender's kind + server-resolved
  // handle (the lesche derives it fresh at read time, so it is already
  // correct) but not the mutable display label, so the header reads that label
  // here; a mention carries ids alone, so the chip resolves label/handle/href
  // here too. Undefined for a sender not in the roster / before the roster
  // lands (handle alone); rebuilt with the roster, so a rename re-renders both.
  const memberById = $derived(
    new Map((conv?.roster?.members ?? []).map((m) => [m.id, m] as const)),
  );

  // Whether the roster has landed -- the chip draws the bare short id until
  // it does.
  const rosterLoaded = $derived(conv?.roster != null);

  // Open the conversation (loads the history). Driven from an effect (not a
  // one-shot onMount) gated on the self participant id (needed to flag own
  // messages -- `mine`). Otherwise a refresh races open() ahead of it resolving
  // and bakes wrong `mine` flags into the rendered lines. open() is idempotent
  // and re-opens from an error state, so re-firing once ready recovers it.
  // Refresh the roster alongside for an at-once member count.
  //
  // The store calls are commands keyed on the participant id, not reactive
  // derivations, so they run inside `untrack`: enter() reads the very entry
  // fields it resets, and the cleanup's leaveRoom() advances them -- tracked,
  // every unread-store write re-fired this effect, and with two instances of
  // the page mounted the interleaved cleanup writes drove the knownSeq
  // ping-pong into effect_update_depth_exceeded (operator live log 2026-09-01).
  $effect(() => {
    if (!archeionSession.participantId) return;
    untrack(() => {
      void roomConversationsStore.open(roomId);
      // Viewing: an explicitly opened room page clears its badge
      // and starts the cursor-write schedule; the cleanup (unmount or a
      // participantId re-fire) syncs the watermark to the conversation's live
      // cursor and flushes the write.
      unreadStore.enter(roomKey(roomId));
      void roomConversationsStore.refreshRoster(roomId);
    });
    return () => untrack(() => unreadStore.leaveRoom(roomId, conv?.lastSeq));
  });

  // Slow poll: the dropped-frame backstop for the room_membership_changed
  // roster nudge + offline-member catch-up. Bounded so a long-idle open tab
  // does not spin. Cleared on unmount.
  onMount(() => {
    const stop = startVisibleInterval(() => {
      // A terminal error (e.g. this member was removed) stops the poll: the
      // room is gone to this user, and refreshing would only loop the failure.
      if (conv?.status === "error") return;
      void roomConversationsStore.refresh(roomId);
      // Refresh the roster (member count + creator badge) at the same cadence;
      // the room_membership_changed SSE is the faster trigger.
      void roomConversationsStore.refreshRoster(roomId);
    }, 10_000);
    return () => stop();
  });

  // Stick to the tail as lines arrive; stop once the user scrolls up to read.
  const scroll = createAutoScroll();
  // Session-key reset must be declared BEFORE the stick effect: effects in
  // the same flush run in declaration order, so a param change resets the
  // controllers before stick() observes the new room's lines. A pending
  // quote is not cleared here: the bar reads it through armedQuoteSeq, which
  // compares scopes only, so a seq armed in the room just left is unreadable
  // here while the state stays: returning to that room shows the strip again,
  // and cancelling or sending are the two paths that clear it. The sentence
  // the reveal spoke goes with them, from this effect's cleanup: destroy()
  // reads back the spoken slot it wrote to flip it, and a tracked body would
  // register that read and dirty itself; a cleanup has no reaction to record.
  $effect(() => {
    void roomId;
    scroll.reset();
    return () => reveal.destroy();
  });
  $effect(() => {
    void conv?.lines.length;
    scroll.stick(conv?.lines.length ?? 0, conv?.lines.at(-1)?.seq);
  });
  // Layout-driven resizes (composer auto-grow, window resize) shrink the
  // container without a scroll event; while following, re-anchor to the
  // tail so a send never visually drags it out of view.
  $effect(() => {
    const vp = scroll.viewport;
    if (!vp) return;
    return scroll.observe(vp);
  });

  // The viewing line tick: lines rendered on the open conversation count as
  // read and coalesce the cursor write into the 5s throttle window: a
  // busy chat must not POST per line; the leave path flushes.
  //
  // Only the rendered-line count is the trigger. noteViewedLines runs inside
  // `untrack`: its internal knownSeq fence reads the same entry field the
  // call may write, and a tracked read there let store writes re-invoke this
  // effect mid-flush (the write/read ping-pong with the mount effect's
  // cleanup above). Lines and lastSeq advance together, so the count alone
  // is a sound tick.
  $effect(() => {
    if (conv?.lines.length === undefined) return;
    untrack(() => {
      unreadStore.noteViewedLines(roomId, conv?.lastSeq ?? null);
    });
  });

  // One scroll-pin controller for the whole room transcript (a single active
  // ResizeObserver across all bubbles); each <MessageBubble> hands its box +
  // action row to it on a raw toggle.
  const togglePin = createTogglePin(() => scroll.viewport);

  // Jumping to the row a quote names: the same row table, scroll and brief
  // mark the 1:1 transcript uses, keyed by this room's seq.
  const reveal = createReveal({
    scope: () => roomId,
    // A jump moves focus without a word, so the controller speaks the row it
    // landed on: the author and time that row's own label names.
    describe: (seq) => {
      const line = conv?.lines.find((row) => row.seq === seq);
      if (line === undefined) return undefined;
      return roomRowAnnouncement(line, {
        time: formatStamp(line.createdAt, ROW_TIME),
        authorLabel: memberById.get(line.senderId)?.label,
      });
    },
  });

  const disabled = $derived(conv?.status !== "open");

  // The togglable member side-panel (online dots driven by the room's live
  // online-member set + roster identity).
  let showMembers = $state(false);

  // The row this send answers, tagged with the room that armed it: one quote
  // at a time, replaced by quoting another row and spent by sending or Escape.
  let pendingQuote = $state<ArmedQuote | null>(null);

  // Reads go through one gate: a seq armed in the room just left numbers a
  // different row here, so the bar, the preview and the send see undefined.
  const armedSeq = $derived(armedQuoteSeq(pendingQuote, roomId));

  // Only a row the room has numbered can be quoted: an optimistic line has no
  // seq the other side could resolve, and a failed one is about to be resent as
  // a different line.
  function quotable(line: RoomLine): boolean {
    return line.seq > 0 && !roomLinePending(line) && !line.failed;
  }
  // A flash timer outliving the transcript would touch a torn-down view:
  // the final teardown runs the reveal's own cleanup -- its mark and
  // sentence with the timer -- and a room swap drops the sentence from its
  // own effect's cleanup, its body resetting the mark.
  $effect(() => () => {
    reveal.destroy();
  });

  const composer = createComposer({
    send: (text) => {
      // One send spends the strips: the picks and the quote are captured here
      // and cleared before the transport await, so a second submit cannot
      // inherit what the user already spent. A failed send keeps both on the
      // optimistic line, which is where the retry reads them back.
      const mentions = picker.picked;
      const quote = armedSeq;
      picker.clear();
      pendingQuote = null;
      return roomConversationsStore.send(roomId, text, { mentions, quote });
    },
    // Busy is not a gate: send renders the optimistic line at once + posts.
    canSubmit: () => conv?.status === "open",
  });

  // Draft storage: rooms key on the room id.
  bindDraft(composer, () => roomDraftKey(roomId));

  // The mention picker: the page holds its state, lib/mentions.ts holds every
  // rule it applies. Candidates are the roster already loaded for the chips --
  // no second fetch, and no popup before the roster lands (there would be
  // nothing to offer, and a half-state popup is worse than none).
  const picker = createMentionPicker({
    // The picks are the composer's, so a swap keeps each room's own and the
    // reader shows the live one: coming back restores what was picked here.
    scope: () => roomId,
    members: () => conv?.roster?.members ?? [],
    draft: () => composer.draft,
    caret: () => composer.caret,
    dropToken: (start, end) => {
      const draft = composer.draft;
      composer.draft = draft.slice(0, start) + draft.slice(end);
      composer.requestCaret(start);
    },
  });

  // The picked members as the same chips a message draws: one vocabulary, one
  // degradation rule. A member who has left the roster keeps the chip the user
  // picked -- it falls back to the roster-missing shape rather than vanishing.
  const pickedChips = $derived(
    mentionChipsOf(
      picker.picked,
      memberById,
      archeionSession.participantId,
      rosterLoaded,
    ),
  );

  // Keys the picker owns come first (it decides from the draft and the caret);
  // the rest of this hook is the backspace-on-empty-draft gesture, which drops
  // the last pick and can never eat a character the user typed.
  function onComposeKey(event: KeyboardEvent): boolean {
    if (picker.key(event)) {
      // Arrows would move the caret, Enter would submit, Tab would leave the
      // field: none of that is wanted while the popup has the key.
      event.preventDefault();
      return true;
    }
    if (
      event.key === "Backspace" &&
      !event.isComposing &&
      composer.draft.length === 0 &&
      picker.picked.length > 0
    ) {
      picker.removeLast();
      return true;
    }
    return false;
  }

  // The row the armed quote points at, re-resolved every render against the
  // window: a row that scrolls out of it drops to the neutral author label
  // instead of showing stale copy.
  const quotedLine = $derived(
    armedSeq === undefined
      ? undefined
      : conv?.lines.find((line) => line.seq === armedSeq),
  );

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

<svelte:head><title>{room_title()}</title></svelte:head>

<svelte:window
  onkeydown={(e) => showMembers && e.key === "Escape" && (showMembers = false)}
/>

<div class="flex flex-col h-full">
  <PageHeader>
    {#snippet title()}
      <p class="text-sm font-semibold truncate">{roomLabel}</p>
      <p class="text-xs opacity-50 truncate">
        <span class="opacity-70">{room_id_label()}</span><span class="font-mono"
          >{roomId}</span
        >
      </p>
    {/snippet}
    {#snippet badges()}
      {#if room?.visibility === "public"}
        <span class="text-xs preset-tonal-surface px-2 py-0.5 rounded-base"
          >{room_public_badge()}</span
        >
      {/if}
      {#if conv?.roster}
        <span class="text-xs opacity-60">
          {conv.roster.members.length === 1
            ? room_member_one({ count: conv.roster.members.length })
            : room_member_other({ count: conv.roster.members.length })}
        </span>
        {#if conv.roster.is_creator}
          <span class="text-xs preset-tonal-surface px-2 py-0.5 rounded-base"
            >{room_creator_badge()}</span
          >
        {/if}
      {/if}
    {/snippet}
    {#snippet actions()}
      <button
        type="button"
        class="size-10 grid place-items-center rounded-base shrink-0 disabled:opacity-60 {showMembers
          ? 'preset-filled-surface-500'
          : 'preset-tonal-surface hover:preset-filled-surface-500'}"
        aria-label={room_toggle_members_aria()}
        aria-pressed={showMembers}
        disabled={!conv || conv.status === "loading"}
        onclick={() => (showMembers = !showMembers)}
      >
        <Users class="size-4" />
      </button>
      <button
        type="button"
        class="size-10 {TONAL_ICON_SURF} shrink-0"
        aria-label={room_settings_aria()}
        onclick={() => navigate(`/rooms/${roomId}/settings`)}
      >
        <Settings class="size-4" />
      </button>
    {/snippet}
  </PageHeader>

  {#if !conv || conv.status === "loading"}
    <div class="flex-1 grid place-items-center p-6">
      <p class="text-sm opacity-60">{room_loading_history()}</p>
    </div>
  {:else}
    <div class="flex-1 min-h-0 flex relative">
      <!-- A jump moves focus without a word, so the row it landed on is
           spoken here. Two slots, written in turn: two jumps that land on rows
           named the same are two separate events, and re-writing one node with
           the same text says nothing at all. -->
      <div class="sr-only" aria-live="polite" aria-atomic="true">
        {reveal.slot === 0 ? (reveal.announcement ?? "") : ""}
      </div>
      <div class="sr-only" aria-live="polite" aria-atomic="true">
        {reveal.slot === 1 ? (reveal.announcement ?? "") : ""}
      </div>
      <!-- UA scroll anchoring is retired here: the controller owns the
           anchoring (stick/observe), and an anchor adjustment racing the
           flex reflow could ship a scroll event with a large
           distance-from-bottom, detaching follow on a mere keystroke. -->
      <div
        class="flex-1 min-h-0 overflow-auto [overflow-anchor:none]"
        bind:this={scroll.viewport}
        onscroll={scroll.onScroll}
      >
        <div class="mx-auto w-full max-w-[80rem] p-4 flex flex-col gap-3">
          {#if conv.lines.length === 0}
            <p class="text-sm opacity-60 text-center mt-8">
              {room_empty()}
            </p>
          {/if}
          {#each conv.lines as line (line.seq)}
            <!-- Still-sending = own optimistic line (see roomLinePending); an
            inbound frame carries no seq yet but is delivered. -->
            {@const pending = roomLinePending(line)}
            {@const quotedSeq = line.quote}
            {@const q = quotes.get(line.seq)}
            {@const quoteTarget = q?.jumpable === true ? quotedSeq : undefined}
            {@const rowLabel = roomMessageLabel(line, {
              time: formatStamp(line.createdAt, ROW_TIME),
              authorLabel: memberById.get(line.senderId)?.label,
            })}
            {@const chips = mentionChipsOf(
              line.mentions,
              memberById,
              archeionSession.participantId,
              rosterLoaded,
            )}
            {@const href = profileHref(
              line.senderKind,
              line.senderHandle,
              line.senderTagmaId,
            )}
            {#snippet extraActions()}
              {#if quotable(line)}
                <QuoteButton
                  onclick={() =>
                    (pendingQuote = { scope: roomId, seq: line.seq })}
                />
              {/if}
            {/snippet}
            <div
              use:reveal.trackRow={line.seq}
              tabindex="-1"
              class="group flex flex-col {line.mine
                ? 'items-end'
                : 'items-start'} {revealMark(reveal.highlightId, line.seq)}"
            >
              <span id={roomRowAnchorId(line.seq)} class="sr-only">
                {rowLabel}
              </span>
              <span
                class="text-xs px-1 mb-0.5 max-w-[80%] min-w-0 flex items-center"
              >
                <SenderIdentity
                  kind={line.senderKind}
                  handle={line.senderHandle}
                  label={memberById.get(line.senderId)?.label}
                  {href}
                />
              </span>
              <MessageRow mine={line.mine}>
                <MessageBubble
                  {extraActions}
                  text={line.text}
                  markdown={line.senderKind === "agent"}
                  mine={line.mine}
                  {pending}
                  bare={pending}
                  pin={togglePin}
                  quote={q}
                  quoteDescribedBy={quoteTarget === undefined
                    ? undefined
                    : roomRowAnchorId(quoteTarget)}
                  mentions={chips}
                  onQuoteClick={quoteTarget === undefined
                    ? undefined
                    : () => reveal.jump(quoteTarget)}
                />
              </MessageRow>
              {#if line.mine && pending && !line.failed}
                <span
                  class="text-xs opacity-50 animate-pulse"
                  aria-label={room_sending_aria()}>··</span
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
                  }}>{common_retry()}</button
                >
              {/if}
            </div>
          {/each}
          {#if conv.status === "error" && conv.error}
            <p class="text-xs text-error-500 dark:text-error-400 text-center">
              {conv.error}
            </p>
          {/if}
        </div>
      </div>
      {#if !scroll.follow}
        <!-- No z-index on purpose: the members panel (z-10, a later
          sibling) must win when open. -->
        <div
          class="absolute inset-x-0 bottom-4 mx-auto flex max-w-[80rem] justify-end px-4 pointer-events-none"
        >
          <ScrollToBottomButton
            missed={scroll.missed}
            onclick={() => scroll.forceBottom()}
          />
        </div>
      {/if}
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
          aria-label={room_members_aria()}
        >
          <div
            class="px-3 py-2 border-b border-surface-200-800 flex items-center justify-between shrink-0"
          >
            <span class="text-sm font-semibold">{room_members()}</span>
            <button
              type="button"
              class="size-7 {TONAL_ICON_SURF}"
              aria-label={room_close_members_aria()}
              onclick={() => (showMembers = false)}
            >
              <X class="size-4" />
            </button>
          </div>
          <div class="flex-1 min-h-0 overflow-auto p-2 flex flex-col gap-1">
            {#if !conv.roster}
              <p class="text-sm opacity-60 px-1 py-2">{common_loading()}</p>
            {:else}
              {#each conv.roster.members as m (m.id)}
                <MemberRow
                  member={m}
                  selfId={archeionSession.participantId}
                  isCreator={conv.roster.is_creator &&
                    m.id === archeionSession.participantId}
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
      disabledNotice={room_unavailable()}
      onSubmitted={() => scroll.forceBottom()}
      onResized={() => scroll.reanchorIfFollowing()}
      onCancelQuote={() => (pendingQuote = null)}
      {onComposeKey}
      mentionAria={{
        open: picker.open,
        activeId:
          picker.activeId === undefined
            ? undefined
            : mentionOptionId(picker.activeId),
      }}
    >
      <!-- The two strips and the popup live in the composer's own slot family:
           the quote this send answers, the members it names, and the
           candidates while an `@` token is open. -->
      {#snippet quoteBar()}
        {#if armedSeq !== undefined}
          <QuoteBar
            author={quotedLine?.senderHandle}
            preview={quotedLine === undefined
              ? undefined
              : previewOf(quotedLine)}
            onCancel={() => (pendingQuote = null)}
          />
        {/if}
      {/snippet}
      {#snippet mentionBar()}
        {#if pickedChips.length > 0}
          <div class="mb-1">
            <MentionChips
              chips={pickedChips}
              onRemove={(id) => picker.remove(id)}
            />
          </div>
        {/if}
      {/snippet}
      {#snippet mentionPicker()}
        {#if picker.open}
          <MentionPicker
            rows={picker.rows}
            activeIndex={picker.activeIndex}
            limitHit={picker.limitHit}
            onPick={(id) => picker.pick(id)}
          />
        {/if}
      {/snippet}
    </Composer>
  {/if}
</div>
