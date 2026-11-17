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
  import { tick, untrack } from "svelte";
  import { BellOff, MessageSquare, Settings, Users, X } from "@lucide/svelte";
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import Composer from "../components/Composer.svelte";
  import MessageBubble from "../components/MessageBubble.svelte";
  import DeleteFailedButton from "../components/DeleteFailedButton.svelte";
  import PageHeader from "../components/PageHeader.svelte";
  import ScrollToBottomButton from "../components/ScrollToBottomButton.svelte";
  import {
    createAboveFoldPin,
    createAutoScroll,
    createTogglePin,
    holdAboveFold,
  } from "../lib/transcript.svelte.ts";
  import { roomQuoteBlocksByLine } from "../lib/quote.ts";
  import { createReveal, revealMark } from "../lib/reveal.svelte.ts";
  import { ROW_TIME } from "../lib/time/stamp.ts";
  import { selectionAtClick } from "../lib/selection.ts";
  import { selectionHoldsText } from "../lib/bubble_click.ts";
  import { createComposer } from "../lib/composer.svelte.ts";
  import { createMentionPicker } from "../lib/mentionPicker.svelte.ts";
  import { mentionOptionId } from "../lib/mentions.ts";
  import { bindDraft } from "../lib/session/drafts.svelte.ts";
  import { roomDraftKey } from "../lib/session/drafts.ts";
  import {
    mentionChipsOf,
    roomRowAnnouncement,
    roomLinePending,
    roomMessageLabel,
    roomRowAnchorId,
    quotableLine,
  } from "../lib/room-message.ts";
  import MemberRow from "../components/rooms/MemberRow.svelte";
  import MentionPicker from "../components/rooms/MentionPicker.svelte";
  import MentionChips from "../components/MentionChips.svelte";
  import MessageRow from "../components/MessageRow.svelte";
  import QuoteButton from "../components/QuoteButton.svelte";
  import SenderIdentity from "../components/rooms/SenderIdentity.svelte";
  import ThreadPanel from "../components/rooms/ThreadPanel.svelte";
  import { roomConversationsStore } from "../lib/session/roomConversations.svelte.ts";
  import { recentStore } from "../lib/session/recent.svelte.ts";
  import { archeionSession } from "../lib/session/archeion.svelte";
  import { roomKey, unreadStore } from "../lib/session/unread.svelte.ts";
  import { navigate } from "../lib/shell/port.ts";
  import { roomThreadPath } from "../lib/shell/routes.ts";
  import { profileHref } from "../lib/room-message.ts";
  import { createRoomSession } from "../lib/rooms/roomSession.svelte.ts";
  import { createThreadWindow } from "../lib/rooms/threadWindow.svelte.ts";
  import { rowParamOf } from "../lib/rooms/rowParam.ts";
  import {
    ensureThreadChoice,
    threadChoiceOf,
  } from "../lib/rooms/threadChoices.svelte.ts";
  import {
    PREVIEW_HANDLE,
    PREVIEW_ROW,
    SCROLL_PILL,
    TONAL_ICON_SURF,
    TRANSCRIPT_COLUMN,
  } from "../lib/classes.ts";
  import {
    drawnRootState,
    rootedRoots,
    threadEntry,
    threadPreview,
    threadGroups,
    threadDiscussion,
    threadIndex,
    type ThreadRootState,
    type ThreadEntry,
    roomRowShape,
  } from "../lib/threads.ts";
  import { lgQuery } from "../lib/shell/breakpoint.ts";
  import { formatStamp } from "../lib/time/stamp.svelte.ts";
  import {
    common_loading,
    common_retry,
    room_title,
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
    chat_transcript_aria,
    room_thread_last_from,
    room_thread_orphan_note,
    room_thread_pending_note,
    room_thread_panel_title,
    room_thread_sheet_description,
    room_thread_replies_one,
    room_thread_replies_other,
    room_thread_view,
    room_thread_muted,
  } from "../paraglide/messages.js";

  let { roomId, at }: { roomId: string; at?: string } = $props();

  const session = createRoomSession(() => roomId);
  const conv = $derived(session.conv);
  // The registry view for this room (name/description/visibility), for the
  // header title. Null only if the room has not surfaced in the registry yet (a
  // brief resolving window).
  const room = $derived(session.room);
  const roomLabel = $derived(session.roomLabel);

  // The quote block each line draws, resolved once per render over the window
  // already on screen. The room transcript keeps every line it has loaded (a
  // history page is capped at 200 rows, but the store appends pages and live
  // frames without trimming), so a lookup per row would be quadratic in it.
  const quotes = $derived(roomQuoteBlocksByLine(conv?.lines ?? []));

  // Each discussion the transcript draws, one entry per head. A reply whose
  // root the loaded window does not hold still gets an entry (see lib/threads.ts):
  // and only that entry: the row that answers a message is drawn in its
  // discussion, never in place of the topic it answers.
  const lineIndex = $derived(threadIndex(conv?.lines ?? []));
  const groups = $derived(threadGroups(conv?.lines ?? [], lineIndex));

  // The standing of every discussion whose head this window holds: the mute
  // mark in the preview is drawn there, so only a rooted group has a reader.
  // A group whose head has not arrived or fell off the window shows no mark,
  // and asking for its standing would spend one request per thread on an
  // answer nothing shows. One read per thread, shared while in flight, an
  // unreadable one remembered as unknown. Nothing the cache holds is tracked
  // here: a landing answer would re-run an effect that asked for it.
  $effect(() => {
    const room = roomId;
    const roots = rootedRoots(groups);
    untrack(() => {
      for (const root of roots) void ensureThreadChoice(room, root);
    });
  });
  // A reply count is a number only for a discussion whose head is loaded and
  // only once the transcript has reached the room's present: replies sit after
  // their head, so a window that reaches the present holds all of them, while
  // a window still walking forward has passed only some of them.
  const countsKnown = $derived(conv?.caughtUp ?? false);

  // The roster keyed by participant id: one pass serves both the sender label
  // the bubble header draws and the members a line mentions. The wire
  // `Participant` on each message carries the sender's kind + server-resolved
  // handle (the lesche derives it fresh at read time, so it is already
  // correct) but not the mutable display label, so the header reads that label
  // here; a mention carries ids alone, so the chip resolves label/handle/href
  // here too. Undefined for a sender not in the roster / before the roster
  // lands (handle alone); rebuilt with the roster, so a rename re-renders both.
  const memberById = $derived(session.memberById);

  // The room's discussion panel, which nothing opens. Every entry point that
  // used to open one navigates to `/rooms/<id>/threads/<root>` instead (a row's
  // bubble, its summary, the reply entry, and the quote control), so the only
  // writer of this root is `closeThread`, and no effect here syncs the window:
  // no read is asked for either. The two gates below therefore stay false and
  // the panel draws nowhere. The panel's code stays where it is.
  let openThreadRoot = $state<number | null>(null);
  // One discussion's pages and the reads that take them: the window, its walk
  // and its generation guards live in the module, because a discussion's own
  // page reads the same thing. Which discussion is open stays here.
  // The panel's body, which owns the row table a jump would land on: the
  // transcript's reveal only covers rows this page draws.
  let threadBody = $state<{
    jump(seq: number): boolean;
    land(seq: number): boolean;
  } | null>(null);

  const threadWindow = createThreadWindow({
    roomId: () => roomId,
    root: () => openThreadRoot,
    lines: () => conv?.lines ?? [],
    onLanded: (seq) => threadBody?.jump(seq),
  });

  // The window's reads, as derived cells: the module hands out getters, and the
  // markup and the grouping below want names.
  const threadLines = $derived(threadWindow.lines);
  const threadHasMore = $derived(threadWindow.hasMore);
  const threadMore = $derived(threadWindow.more);
  const threadWalking = $derived(threadWindow.walking);
  const threadPages = $derived(threadWindow.pages);
  const threadLoading = $derived(threadWindow.loading);
  const threadUnreadable = $derived(threadWindow.unreadable);

  /** Ask again for a first page the panel could not read. */
  const retryThread = threadWindow.retryThread;

  /** Take the page below what the panel holds. */
  const loadMoreThread = threadWindow.loadMoreThread;

  /** The panel's retry after a page failed: a walk that ran into the failure
   *  carries on rather than leaving the reader to find the row. */
  const retryMore = threadWindow.retryMore;

  /** Carry on a walk that stopped at the page cap. */
  const continueWalk = threadWindow.continueWalk;

  /** Send one reply into the open discussion, then follow it to the newest
   *  row so the confirmed echo takes the pending line's place. */
  const sendReply = threadWindow.sendReply;

  /** The panel's way back to the discussion's newest reply: the same walk a
   *  reply takes, so it stops at the same page cap. */
  async function returnToNewest(): Promise<void> {
    if (openThreadRoot === null) return;
    await threadWindow.walkTo("newest");
  }

  /** The words a room entry uses for its discussion: the count, and the last
   *  sender. Built in one place so the sentence the eye reads and the label a
   *  screen reader hears are the same words in the same order -- an entry whose
   *  name said something the header does not would be two accounts of one row. */
  function threadSummaryText(
    summary: ThreadEntry,
    muted: boolean,
  ): {
    readonly head: string;
    readonly tail?: string;
    readonly label: string;
  } {
    const head =
      summary.count === undefined
        ? room_thread_view()
        : summary.count === 1
          ? room_thread_replies_one({ count: summary.count })
          : room_thread_replies_other({ count: summary.count });
    const tail =
      summary.last === undefined
        ? undefined
        : room_thread_last_from({ handle: summary.last.senderHandle });
    const sentence = tail === undefined ? head : head + " · " + tail;
    // The mark the eye reads (a bell with a slash) and the word a screen reader
    // hears are one fact: a muted discussion says so in the entry's name.
    const label = muted ? sentence + " · " + room_thread_muted() : sentence;
    return { head, tail, label };
  }

  /** Close the panel. Nothing opens it (see the root's comment above); the
   *  sheet's dismiss still calls this, and it drops all the window read. */
  function closeThread(): void {
    openThreadRoot = null;
    threadWindow.clear();
  }

  // Where the open discussion's root sits relative to the loaded window, read
  // from the same grouping the transcript draws: the entry and the panel header
  // must never say different things about the same discussion. A root the
  // panel draws itself is rooted whatever the window holds: the discussion read
  // reaches roots the room's loaded window never had (see `drawnRootState`).
  const openThreadState = $derived<ThreadRootState>(
    openThreadRoot === null
      ? "unknown"
      : drawnRootState(
          openThreadRoot,
          threadLines,
          groups.find((group) => group.root === openThreadRoot)?.rootState,
        ),
  );

  // The note under a discussion's entry follows the panel while that discussion
  // is open: the panel draws rows the room's grouping cannot see, so without
  // this an entry saying the discussion starts before the loaded window would
  // sit beside a header showing its head. The count and the last sender stay
  // the grouping's.
  const entryRootStates = $derived(
    new Map(
      groups.map((group) => [
        group.root,
        group.root === openThreadRoot ? openThreadState : group.rootState,
      ]),
    ),
  );

  // Why the reader is in the open discussion, as the panel header says it:
  // null when the read could not see the whole discussion, so a half
  // discussion never explains the wrong reason.
  const membership = $derived(threadWindow.membership);

  /** The discussion a quote target belongs to: a head is its own, a reply is
   *  filed under another row. */
  function quotedRoot(seq: number): number | undefined {
    const line = lineIndex.get(seq);
    return line === undefined ? undefined : threadDiscussion(line, lineIndex);
  }

  /** A quote block's jump. A head is drawn here, so scroll to it; a reply is
   *  drawn in the discussion that holds it, so the reader goes there and lands
   *  on the row. */
  function jumpToQuote(seq: number): void {
    const root = quotedRoot(seq);
    if (root === undefined) return;
    if (root === seq) {
      reveal.jump(seq);
      return;
    }
    void navigate(roomThreadPath(roomId, root, { at: seq }));
  }

  /** The id that describes a quote block: the target row's own hidden label,
   *  and only while this view draws that row. A reply is drawn on its
   *  discussion's page, so a block naming one points nowhere here. */
  function quoteAnchor(seq: number): string | undefined {
    return quotedRoot(seq) === seq ? roomRowAnchorId(seq) : undefined;
  }

  // Width decides where the panel lives: a column beside the room on lg+, a
  // full-screen sheet below it. The listener re-evaluates on resize and is
  // removed with the effect.
  const lgMql = matchMedia(lgQuery);
  let wide = $state(lgMql.matches);
  $effect(() => {
    const onChange = (event: MediaQueryListEvent) => (wide = event.matches);
    lgMql.addEventListener("change", onChange);
    return () => lgMql.removeEventListener("change", onChange);
  });

  // Whether the roster has landed -- the chip draws the bare short id until
  // it does.
  const rosterLoaded = $derived(session.rosterLoaded);

  // Open the conversation (loads the history) through the room session, gated
  // on the self participant id: own `mine` flags need it, and a refresh racing
  // open() ahead of it would bake wrong flags into the rendered lines. open()
  // is idempotent and re-opens from an error state, so re-firing once ready
  // recovers it.
  //
  // The effect learns the room id through session.sync(), which reads the
  // getter untracked only around its store commands: the page instance
  // survives a room-id change (the route keys on the pattern, not the id), so
  // a change must open the room that is now on screen.
  $effect(() => {
    if (!archeionSession.participantId) return;
    session.sync();
    return () => untrack(() => session.stop());
  });

  // Stick to the tail as lines arrive; stop once the user scrolls up to read.
  const scroll = createAutoScroll();

  /** The composer instance, for the case where no row is left to land on:
   *  an emptied transcript after the last line is discarded. */
  let composerRef: { focusInput(): boolean } | null = $state(null);

  /** Run the discard, then put focus where the reader continues: the row above
   *  the one that left, else the composer, else the transcript itself. The
   *  thread place is read before the discard runs: the store drops the row as
   *  it answers, so a lookup afterwards names nothing and sends the reader to
   *  the top. The place is a discussion's, not a row's, because the transcript
   *  draws one entry per discussion and only a head can carry a failed send. */
  function discardRow(removed: number): boolean {
    const at = groups.findIndex((group) => group.head.seq === removed);
    if (!roomConversationsStore.discard(roomId, removed)) return false;
    landFocus(at > 0 ? (groups[at - 1]?.head.seq ?? null) : null);
    return true;
  }

  /** Drop a failed reply from the panel: the store's reducer alone, because
   *  the panel keeps its own focus (the transcript behind it is not where the
   *  reader is). */
  function discardReplyLine(seq: number): boolean {
    return roomConversationsStore.discard(roomId, seq);
  }

  /** Focus the row `id`, or the next thing that can speak for the transcript:
   *  the composer, then the viewport itself. A disabled composer cannot take
   *  focus, and focus must not fall through to the document. */
  function landFocus(id: number | null): void {
    if (id !== null && reveal.land(id)) return;
    if (composerRef?.focusInput() !== true) {
      scroll.viewport?.focus({ preventScroll: true });
    }
  }
  // Session-key reset must be declared BEFORE the stick effect: effects in
  // the same flush run in declaration order, so a param change resets the
  // controllers before stick() observes the new room's lines. The sentence the
  // reveal spoke goes with them, from this effect's cleanup: destroy() reads
  // back the spoken slot it wrote to flip it, and a tracked body would register
  // that read and dirty itself; a cleanup has no reaction to record.
  $effect(() => {
    void roomId;
    // The page instance survives a room-id change (the route keys on the
    // pattern, not the id), so the discussion panel closes with the room it was
    // opened in: its root numbers rows of a room no longer on screen.
    closeThread();
    scroll.reset();
    return () => {
      reveal.destroy();
    };
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
  // The same tick stamps the room in the recents ledger: reading a room is
  // activity even when it had no badge to clear.
  $effect(() => {
    if (conv?.lines.length === undefined) return;
    untrack(() => {
      unreadStore.noteViewedLines(roomId, conv?.lastSeq ?? null);
      recentStore.note(roomKey(roomId));
    });
  });

  // One scroll-pin controller for the whole room transcript (a single active
  // ResizeObserver across all bubbles); each <MessageBubble> hands its box +
  // action row to it on a raw toggle.
  const togglePin = createTogglePin(() => scroll.viewport);

  // The transcript turns native anchoring off, so a discussion row that gains a
  // reply preview while the reader is further down would push what they are
  // reading down with it. One growth pin for the whole transcript, as with the
  // toggle above; the rows that can grow hang it on themselves with `use:`. A row
  // takes one of the two: `pin` follows a click for a moment, this one stays
  // subscribed, and two writers on one `scrollTop` would double-correct.
  const aboveFoldPin = createAboveFoldPin(() => scroll.viewport);

  // Jumping to the row a quote names: the same row table, scroll and brief
  // mark the 1:1 transcript uses, keyed by this room's seq.
  const reveal = createReveal({
    scope: () => roomId,
    // A jump moves focus without a word, so the controller speaks the row it
    // landed on: the author and time that row's own label names.
    describe: (seq) => {
      const line = lineIndex.get(seq);
      if (line === undefined) return undefined;
      return roomRowAnnouncement(line, {
        time: formatStamp(line.createdAt, ROW_TIME),
        authorLabel: memberById.get(line.senderId)?.label,
      });
    },
  });

  // A deep link that names a row: a discussion's own page sends its room
  // step here, and the reader expects to land on that discussion rather than
  // the room's newest line. Landing happens once per room and row, after the
  // history lands -- a refresh of the same link must not pull the reader
  // back. A row the loaded window does not hold leaves the reader at the
  // newest line, which is where the transcript opens anyway.
  let landedKey: string | null = null;
  $effect(() => {
    const key = `${roomId}:${at ?? ""}`;
    if (landedKey === key || !conv?.historyLoaded) return;
    landedKey = key;
    const landAt = rowParamOf(at);
    if (landAt === null) return;
    untrack(() => reveal.land(landAt));
  });

  const disabled = $derived(!conv?.historyLoaded);

  // The togglable member side-panel (online dots driven by the room's live
  // online-member set + roster identity).
  let showMembers = $state(false);

  // A flash timer outliving the transcript would touch a torn-down view:
  // the final teardown runs the reveal's own cleanup -- its mark and
  // sentence with the timer -- and a room swap drops the sentence from its
  // own effect's cleanup, its body resetting the mark.
  $effect(() => () => {
    reveal.destroy();
  });

  const composer = createComposer({
    send: (text) => {
      // The picks are the send's: captured here and cleared before the
      // transport awaits, so a second submit cannot inherit what the user
      // already spent. A failed send keeps them on the optimistic line, which
      // is where the retry reads them back.
      const mentions = picker.picked;
      picker.clear();
      return roomConversationsStore.send(roomId, text, { mentions });
    },
    // Busy is not a gate: send renders the optimistic line at once + posts.
    canSubmit: () => conv?.historyLoaded ?? false,
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
    {#snippet threadPanel()}
      <ThreadPanel
        root={openThreadRoot ?? 0}
        {roomId}
        lines={threadLines}
        roomLines={conv.lines}
        {memberById}
        selfId={archeionSession.participantId}
        {rosterLoaded}
        loading={threadLoading}
        failed={threadUnreadable}
        hasMore={threadHasMore}
        pages={threadPages}
        more={threadMore}
        walking={threadWalking}
        {membership}
        onClose={closeThread}
        onReply={sendReply}
        onRetry={retryThread}
        onRetryMore={retryMore}
        onLoadMore={loadMoreThread}
        onContinueWalk={continueWalk}
        onReturnToNewest={returnToNewest}
        onDiscard={discardReplyLine}
        bind:this={threadBody}
      />
    {/snippet}
    <div class="flex-1 min-h-0 flex">
      <div class="flex-1 min-h-0 flex flex-col min-w-0">
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
          <!-- The scroll box is the last place a discard's landing can put the
           focus, when the room empties while the composer cannot take it, so
           it is a focus target of its own, outside the tab order, with a name
           of its own: a click on the blank space below the last line focuses
           it, which is the price of the fallback having somewhere audible to
           land. -->
          <div
            class="flex-1 min-h-0 overflow-auto [overflow-anchor:none]"
            bind:this={scroll.viewport}
            tabindex="-1"
            role="group"
            aria-label={chat_transcript_aria()}
            onscroll={scroll.onScroll}
          >
            <div class={TRANSCRIPT_COLUMN}>
              {#if conv.lines.length === 0}
                <p class="text-sm opacity-60 text-center mt-8">
                  {room_empty()}
                </p>
              {/if}
              {#each groups as group (group.root)}
                {@const line = group.head}
                <!-- Still-sending = own optimistic line (see roomLinePending); an
            inbound frame carries no seq yet but is delivered. -->
                {@const pending = roomLinePending(line)}
                {@const quotedSeq = line.quote}
                {@const q = quotes.get(line.seq)}
                {@const quoteTarget =
                  q?.jumpable === true ? quotedSeq : undefined}
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
                {@const shape = roomRowShape(group)}
                {@const summary = threadEntry(group, countsKnown)}
                {@const preview = threadPreview(group)}
                {@const muted =
                  threadChoiceOf(roomId, group.root)?.effective === "muted"}
                {@const words = threadSummaryText(summary, muted)}
                {#snippet extraActions()}
                  {#if quotableLine(line)}
                    <QuoteButton
                      onclick={() =>
                        void navigate(
                          roomThreadPath(roomId, group.root, {
                            quote: line.seq,
                          }),
                        )}
                    />
                  {/if}
                {/snippet}

                <!-- The discussion a row IS: the summary sits inside the
                     bubble, and the whole box opens the discussion. -->
                <!-- A failed line is never an entry: a discussion's head is a row
                     the room numbered, and a failed line of the reader's own has
                     no number yet. -->
                <!-- Copy both surfaces share. The stamp is the one difference:
                     an entry button is the only place a reader learns when the
                     last reply landed (no bubble body sits under it), while a
                     footer sits under the full text, where a stamp would only
                     squeeze it. -->
                {#snippet threadSummaryLines(withStamp: boolean)}
                  <span class="font-medium shrink-0">{words.head}</span>
                  {#if words.tail !== undefined}
                    <span class="opacity-60 truncate">
                      · {words.tail}{#if withStamp && summary.last !== undefined}
                        · {formatStamp(summary.last.createdAt, ROW_TIME)}{/if}
                    </span>
                  {/if}
                {/snippet}
                {#snippet threadSummary()}
                  <!-- The entry says what its discussion holds and shows the
                     start of it; the whole strip opens it. The name a screen
                     reader hears is the header's own sentence (`words.label`),
                     so nothing is announced that the eye is not reading. A row
                     leads with the body and trails the author at the row's own
                     size, and no divider separates the strip from the message:
                     the ring already bounds the block. Sizes, the cap and the
                     opacity live with PREVIEW_ROW and PREVIEW_HANDLE. -->
                  <button
                    type="button"
                    aria-label={words.label}
                    class="mt-2 w-full min-w-0 pt-1.5 text-left text-xs flex flex-col gap-1 cursor-pointer"
                    onclick={(event) => {
                      if (!selectionHoldsText(selectionAtClick(event))) {
                        void navigate(roomThreadPath(roomId, group.root));
                      }
                    }}
                  >
                    <span class="flex items-center gap-1.5 min-w-0">
                      <MessageSquare
                        class="size-3.5 shrink-0"
                        aria-hidden="true"
                      />
                      {#if muted}
                        <BellOff class="size-3.5 shrink-0" aria-hidden="true" />
                      {/if}
                      {@render threadSummaryLines(false)}
                    </span>
                    {#if preview.rows.length > 0}
                      <span class="flex flex-col gap-1 min-w-0">
                        {#each preview.rows as row (row.seq)}
                          <span class={PREVIEW_ROW}>
                            <span class="line-clamp-2 break-words min-w-0"
                              >{row.text}</span
                            >
                            <span
                              class={PREVIEW_HANDLE}
                              title={row.senderHandle}>{row.senderHandle}</span
                            >
                          </span>
                        {/each}
                      </span>
                    {/if}
                  </button>
                {/snippet}
                <div
                  use:reveal.trackRow={line.seq}
                  use:holdAboveFold={shape === "thread"
                    ? aboveFoldPin
                    : undefined}
                  tabindex="-1"
                  class="group flex flex-col {line.mine
                    ? 'items-end'
                    : 'items-start'} {revealMark(reveal.highlightId, line.seq)}"
                >
                  {#if shape !== "entry"}
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
                        footer={shape === "thread" ? threadSummary : undefined}
                        entry={shape === "thread"
                          ? () => {
                              void navigate(roomThreadPath(roomId, group.root));
                            }
                          : undefined}
                        noActionRow={pending || shape === "thread"}
                        pin={shape === "thread" ? undefined : togglePin}
                        quote={q}
                        quoteDescribedBy={quoteTarget === undefined
                          ? undefined
                          : quoteAnchor(quoteTarget)}
                        mentions={chips}
                        onQuoteClick={quoteTarget === undefined
                          ? undefined
                          : () => jumpToQuote(quoteTarget)}
                      />
                    </MessageRow>
                  {/if}
                  <!-- The send's own state, not the row's body: a reply whose
                       discussion head is out of the window is not drawn here,
                       but its Retry and its way out still are. -->
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
                    <DeleteFailedButton
                      onConfirm={() => discardRow(line.seq)}
                    />
                  {/if}
                  {#if shape === "entry"}
                    <button
                      type="button"
                      class="mt-1 text-xs px-2 py-1 rounded-base preset-tonal-surface hover:preset-filled-surface-500 flex items-center gap-1.5 max-w-[80%] min-w-0"
                      onclick={(event) => {
                        if (!selectionHoldsText(selectionAtClick(event))) {
                          void navigate(roomThreadPath(roomId, group.root));
                        }
                      }}
                    >
                      {@render threadSummaryLines(true)}
                    </button>
                  {/if}
                  {#if entryRootStates.get(group.root) === "older"}
                    <span class="text-xs opacity-60 mt-0.5 px-1">
                      {room_thread_orphan_note()}
                    </span>
                  {:else if entryRootStates.get(group.root) === "pending"}
                    <span class="text-xs opacity-60 mt-0.5 px-1">
                      {room_thread_pending_note()}
                    </span>
                  {/if}
                </div>
              {/each}
              {#if conv.status === "error" && conv.error}
                <p
                  class="text-xs text-error-500 dark:text-error-400 text-center"
                >
                  {conv.error}
                </p>
              {/if}
            </div>
          </div>
          {#if !scroll.follow}
            <!-- The bare pill, not the raised one: the members panel is a
          later sibling at the same level and must win when open. -->
            <div class={SCROLL_PILL}>
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
          bind:this={composerRef}
          {composer}
          {disabled}
          pendingCount={0}
          disabledNotice={room_unavailable()}
          onSubmitted={() => scroll.forceBottom()}
          onResized={() => scroll.reanchorIfFollowing()}
          {onComposeKey}
          mentionAria={{
            open: picker.open,
            activeId:
              picker.activeId === undefined
                ? undefined
                : mentionOptionId(picker.activeId),
          }}
        >
          <!-- The strips and the popup live in the composer's own slot family:
           the members this send names, and the candidates while an `@` token
           is open. -->
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
      </div>
      <!-- The aside the panel would sit in. The gate never opens: its root is
           null from the first render on (see the root's comment above). -->
      {#if wide && openThreadRoot !== null}
        <aside
          class="w-96 max-w-[40vw] shrink-0 min-h-0 flex flex-col border-l border-surface-200-800 bg-surface-100-900"
          aria-label={room_thread_panel_title()}
        >
          {@render threadPanel()}
        </aside>
      {/if}
    </div>
    <!-- The sheet the same panel would fill. The gate never opens, as above. -->
    {#if !wide && openThreadRoot !== null}
      <Dialog
        open={true}
        onOpenChange={(event) => {
          if (!event.open) closeThread();
        }}
      >
        <Portal>
          <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
          <Dialog.Positioner class="fixed inset-0 z-50">
            <Dialog.Content
              class="h-full w-full flex flex-col bg-surface-100-900"
            >
              <Dialog.Title class="sr-only">
                {room_thread_panel_title()}
              </Dialog.Title>
              <Dialog.Description class="sr-only">
                {room_thread_sheet_description()}
              </Dialog.Description>
              {@render threadPanel()}
            </Dialog.Content>
          </Dialog.Positioner>
        </Portal>
      </Dialog>
    {/if}
  {/if}
</div>
