<script lang="ts">
  // The reading surface for one discussion: the pages of it that have been
  // read, in order, with the same bubble a transcript row draws. It is a
  // leaf: its host says which discussion is open, the window holds the pages
  // and walks down them, and its own callbacks ask for both. It draws that
  // state and never guesses a count or a reason.
  //
  // It scrolls itself. Opening lands on the discussion's start, because the
  // first page is the stretch the root sits on and the reader comes here to
  // find a row; a reply of their own, or the pill, brings them to the newest
  // row. The bottom edge asks for the next page, and holds the states of a
  // walk that stopped or of a reply the discussion does not hold.
  // A page the panel asked for lands below the reader, who stays where
  // they were reading; a row the room brings in is news, and is followed
  // while they sit on the newest one.
  import { tick } from "svelte";
  import ScrollToBottomButton from "../ScrollToBottomButton.svelte";
  import DeleteFailedButton from "../DeleteFailedButton.svelte";
  import MessageBubble from "../MessageBubble.svelte";
  import MessageRow from "../MessageRow.svelte";
  import SenderIdentity from "./SenderIdentity.svelte";
  import QuoteButton from "../QuoteButton.svelte";
  import QuoteBar from "../QuoteBar.svelte";
  import Composer from "../Composer.svelte";
  import { createComposer } from "../../lib/composer.svelte.ts";
  import { bindDraft } from "../../lib/session/drafts.svelte.ts";
  import { roomThreadDraftKey } from "../../lib/session/drafts.ts";
  import {
    createAutoScroll,
    createTogglePin,
  } from "../../lib/transcript.svelte.ts";
  import { createReveal, revealMark } from "../../lib/reveal.svelte.ts";
  import { ROW_TIME } from "../../lib/time/stamp.ts";
  import { SCROLL_PILL_RAISED, TRANSCRIPT_COLUMN } from "../../lib/classes.ts";
  import { formatStamp } from "../../lib/time/stamp.svelte.ts";
  import {
    mentionChipsOf,
    profileHref,
    roomLinePending,
    roomMessageLabel,
    roomRowAnnouncement,
    roomThreadRowAnchorId,
    quotableLine,
    type MentionTarget,
    type RoomLine,
  } from "../../lib/room-message.ts";
  import { previewOf, roomQuoteBlocksByLine } from "../../lib/quote.ts";
  import { type ThreadMore } from "../../lib/threads.ts";
  import {
    common_retry,
    room_thread_empty,
    room_thread_failed,
    room_thread_loading,
    room_thread_more_failed,
    room_thread_more_loading,
    room_thread_more_unreachable,
    room_thread_panel_title,
    room_thread_walk_continue,
    room_thread_walk_missing,
    room_thread_walk_progress,
    room_thread_walk_stopped,
    room_unavailable,
  } from "../../paraglide/messages.js";

  let {
    root,
    roomId,
    lines,
    roomLines,
    memberById,
    selfId,
    rosterLoaded,
    loading,
    failed,
    hasMore,
    more,
    walking,
    pages,
    onReply,
    onRetry,
    onRetryMore,
    onLoadMore,
    onContinueWalk,
    onReturnToNewest,
    onDiscard,
    onResend,
    onQuoteJump,
    armAt,
    onArmSettled,
  }: {
    /** Which discussion this panel is reading. A change is a different
     *  discussion: the draft and the scroll belong to the one that left. */
    root: number;
    /** The room this discussion is drawn from. The draft's key needs it: a
     *  discussion is numbered from its own room's rows. */
    roomId: string;
    /** The rows of the discussion this panel has read, in order. */
    lines: readonly RoomLine[];
    /** The room's loaded lines. A quote in this discussion may name one of
     *  them instead of a reply: the panel resolves it there, because stating
     *  that the message is missing would be false while the room draws it. */
    roomLines: readonly RoomLine[];
    memberById: ReadonlyMap<string, MentionTarget>;
    selfId: string | null;
    rosterLoaded: boolean;
    loading: boolean;
    failed: boolean;
    /** TRUE while the window the panel holds ends on a full page, so the
     *  discussion goes on below it. The bottom edge asks for that page; a
     *  reader walking down to a row sees the same flag stop the walk. */
    hasMore: boolean;
    /** What the foot of the panel says, as one value: a page being taken, a
     *  page that failed, a walk that stopped at the page cap, a row no page
     *  drew, or nothing to say. One slot, one branch. */
    more: ThreadMore;
    /** A walk is reading down the discussion: the panel follows it, so
     *  the pages it takes arrive on screen instead of as a count off it. */
    walking: boolean;
    /** How many pages of this discussion have landed, the first one included.
     *  A page is content the panel asked for: it lands below the reader and
     *  leaves them where they were reading. A row the room brings in is news,
     *  and is followed while they are on the newest row. */
    pages: number;
    /** Open the next page below the window. */
    onLoadMore: () => Promise<unknown>;
    /** Ask again for a page that failed, and carry a walk on to its row if
     *  one was still walking when the page was lost. */
    onRetryMore: () => Promise<void>;
    /** Carry on the walk that stopped at the page cap. */
    onContinueWalk: () => Promise<void>;
    /** Read down to the discussion's newest row: the pill, and the way back
     *  after a reply of the reader's own. */
    onReturnToNewest: () => Promise<void>;
    /** Ask again for a first page the panel could not read. */
    onRetry: () => Promise<void>;
    /** Send one reply into this discussion. Rejects on failure; the panel
     *  keeps the text and shows the shared send-failure line. */
    onReply: (text: string, quote?: number) => Promise<void>;
    /** Drop a failed reply. Answers whether it left; the panel keeps the
     *  focus, because the transcript behind it is not where the reader is. */
    onDiscard: (seq: number) => boolean;
    /** Send a failed reply again. Omitted = the row offers only its way out,
     *  which is what a host with no store behind it can honour. */
    onResend?: (line: RoomLine) => void;
    /** Land on a row this panel draws. A quote of a room message outside
     *  the discussion has nowhere here to land, so it stays inert. */
    onQuoteJump?: (seq: number) => void;
    /** A row to answer as soon as this surface draws it: the discussion page
     *  hands over the row a room's quote button named. The row may sit on a
     *  page further down, so the arm waits for it rather than for the mount. */
    armAt?: number;
    /** Called when the box stops answering that row -- sent, cancelled, or
     *  another discussion taking its place -- so the page clears the link
     *  that asked for the arm. */
    onArmSettled?: () => void;
  } = $props();

  const quotes = $derived(roomQuoteBlocksByLine(lines, roomLines));
  // The seqs this panel draws: what a quote can be described by or jump to.
  const drawnSeqs = $derived(new Set(lines.map((line) => line.seq)));

  // This surface's own row table: the window may draw a row the room's
  // transcript never did, so a quote that names one of them lands here. Scoped
  // to the discussion, because the next one numbers its replies from its own
  // scale.
  const reveal = createReveal({
    scope: () => String(root),
    describe: (seq) => {
      const line = lines.find((row) => row.seq === seq);
      if (line === undefined) return undefined;
      return roomRowAnnouncement(line, {
        time: formatStamp(line.createdAt, ROW_TIME),
        authorLabel: memberById.get(line.senderId)?.label,
      });
    },
  });
  $effect(() => () => reveal.destroy());

  /** Land on one of this surface's rows: a jump from outside. */
  export function jump(seq: number): boolean {
    return reveal.jump(seq);
  }

  /** Put the reader on a row without a sentence: the landing after a send, or
   *  after a discard. */
  export function land(seq: number): boolean {
    return reveal.land(seq);
  }

  // The reply box is the shared composer, so a reply is typed in the same
  // field as a room message: one card, one send state, one failure copy. What
  // stays this surface's own is the row a send answers (below) and the draft's
  // key, which names the discussion rather than the room.
  let composerRef: { focusInput(): boolean } | null = $state(null);
  const composer = createComposer({
    send: (text) => sendReply(text),
    canSubmit: () => !loading && !failed,
  });
  bindDraft(composer, () => roomThreadDraftKey(roomId, root));
  // The reply this panel's next send answers: one at a time, set from a row's
  // action row, cleared by sending it, by cancelling, and by opening another
  // discussion. A send that fails keeps it, so the retry answers the same row.
  // The panel keeps it rather than the page, so the quote travels with the box
  // the reader is typing in.
  let armedQuote = $state<RoomLine | undefined>(undefined);
  // Bumped on every arm and every drop. A send remembers the generation it
  // started in and clears only the quote it carried: arming again mid-flight
  // is a new arm, even on the same row, and the next reply still answers it.
  let armGeneration = 0;

  // A row handed in from outside: the discussion page reads `?quote=` before
  // it has read anything, and the row may sit on a page further down, so the
  // arm waits for the row rather than for the mount. The pair is the key, not
  // the mount: arming again after a cancel is a new arm, and a value that
  // leaves the surface forgets what it served.
  let armedFor: number | null = null;
  let armedRoot: number | null = null;
  $effect(() => {
    const seq = armAt;
    if (seq === undefined) {
      armedFor = null;
      return;
    }
    if (armedFor === seq && armedRoot === root) return;
    const line = lines.find((row) => row.seq === seq);
    if (line === undefined) return;
    armedFor = seq;
    armedRoot = root;
    armQuote(line);
  });

  /** Answer this row with the next reply. */
  function armQuote(line: RoomLine): void {
    armedQuote = line;
    armGeneration += 1;
  }

  /** Stop answering it, without telling the page: the row belongs to a
   *  discussion that is leaving. */
  function disarmQuote(): void {
    armedQuote = undefined;
    armGeneration += 1;
  }

  /** The reader drops the row this box was answering: the page that sent the
   *  arm hears it, so a refresh does not answer the row again. */
  function cancelArm(): void {
    disarmQuote();
    onArmSettled?.();
  }

  // A reader paging forward through a discussion is not reading a chat tail:
  // a page that lands below them must not pull them to its bottom. Following
  // is for the three places that ask for it -- the pill, an own send, and a
  // walk -- and each of those moves the viewport itself.
  const scroll = createAutoScroll({ stickOnAppend: false });
  // The raw toggle swaps the bubble for its source, which changes the row
  // height: the same pin the chat and room transcripts use keeps the row
  // where the reader had it, instead of letting the viewport jump.
  const togglePin = createTogglePin(() => scroll.viewport);
  // Set while the panel is waiting for its first page, so the page that lands
  // puts the reader at the discussion's start instead of its tail. A plain
  // variable, not state: it steers one scroll, and nothing draws from it.
  let landAtTop = false;
  let sentinel = $state<HTMLDivElement | null>(null);

  // A different discussion brings its own start: the reading position restarts
  // at the top, on the page the root sits on. The draft changes with it too,
  // through the composer's key, which names the discussion.
  $effect(() => {
    void root;
    disarmQuote();
    landAtTop = true;
    scroll.reset();
  });

  // A page the panel asked for lands below the reader: it leaves them where
  // they were reading. A row the room brings in is news: followed while they
  // sit on the newest row, counted for the pill while they read above it. The
  // page counts its own landings, which is how the two tell apart.
  let landsSeen = 0;
  $effect(() => {
    void root;
    const count = lines.length;
    if (landAtTop) {
      if (loading || failed) return;
      landAtTop = false;
      landsSeen = pages;
      if (count === 0) return;
      // The rows are drawn from here on, so the top is where to put the
      // reader: the root is the reason the panel opened.
      void tick().then(() => {
        if (!scroll.viewport) return;
        scroll.viewport.scrollTop = 0;
        // The panel sits above its tail by design, so the rows that land
        // below must not pull the reader down to them. The page in front
        // of them is the baseline: it is not something they missed.
        scroll.detach(count, lines.at(-1)?.seq);
      });
      // The landing scroll is pending, and the reader is not following while
      // it is: this run must not also stick to the tail.
      return;
    }
    if (walking) {
      landsSeen = pages;
      // A walk is taking pages below the reader: keep them on the newest
      // row, so the rows arrive where they can be read.
      scroll.forceBottom();
      return;
    }
    if (pages !== landsSeen) {
      // The page the panel asked for: the reader carries on from where they
      // were reading, and the stretch below them is not a miss.
      landsSeen = pages;
      scroll.stick(count, lines.at(-1)?.seq);
      return;
    }
    scroll.arrived(count, lines.at(-1)?.seq);
  });

  // The bottom edge: reaching it asks for the next page of the discussion.
  // The scroll container is the observer's root -- with the implicit
  // viewport root the observer skips recalculating on inner-container
  // scrolling, and the sentinel never fires (the transcript learned this
  // live). A failed page stops the asking until the reader says so.
  $effect(() => {
    const node = sentinel;
    const viewport = scroll.viewport;
    if (!node || !viewport) return;
    const io = new IntersectionObserver(
      (entries) => {
        if (!entries[0]?.isIntersecting) return;
        if (!hasMore || more !== "idle") return;
        void onLoadMore();
      },
      { root: viewport },
    );
    io.observe(node);
    return () => io.disconnect();
  });

  /** The pill, and the way back after a reply: read down to the newest row,
   *  then land on it. A walk that stopped or found nothing leaves the panel
   *  saying so rather than jumping to a row it never reached. */
  async function returnToNewest(): Promise<void> {
    await onReturnToNewest();
    scroll.forceBottom();
  }

  /** Send one reply, and spend only the quote it carried: one armed while the
   *  send was in flight answers the next reply and stays on screen. A
   *  discussion closed mid-flight bumped the generation, so the arm installed
   *  for its successor is left alone. */
  async function sendReply(text: string): Promise<void> {
    const sentGeneration = armGeneration;
    const sent = armedQuote;
    await onReply(text, sent?.seq);
    if (armGeneration === sentGeneration) {
      armedQuote = undefined;
      if (sent !== undefined) onArmSettled?.();
    }
    // The reply is the discussion's newest row: the page brought the panel
    // down to it, so the reader sees what they just wrote.
    scroll.forceBottom();
  }

  /** A quote block inside this discussion lands on the row it names. A surface
   *  may take the move over -- a discussion page routes a quote of another
   *  discussion -- and otherwise the row is drawn here. */
  function jumpToQuoted(seq: number): void {
    if (onQuoteJump !== undefined) onQuoteJump(seq);
    else reveal.jump(seq);
  }

  /** A discarded reply takes its own row with it; the composer is where the
   *  reader continues, and the failed trigger never receives focus again. */
  function discard(seq: number): boolean {
    const left = onDiscard(seq);
    if (left) composerRef?.focusInput();
    return left;
  }
</script>

<div class="relative flex-1 min-h-0 flex flex-col">
  <div
    class="flex-1 min-h-0 overflow-auto [overflow-anchor:none]"
    bind:this={scroll.viewport}
    onscroll={scroll.onScroll}
    role="group"
    aria-label={room_thread_panel_title()}
  >
    <div class={TRANSCRIPT_COLUMN} role="log">
      {#if loading}
        <p class="text-sm opacity-60">{room_thread_loading()}</p>
      {:else if failed}
        <p class="text-sm text-error-500 dark:text-error-400">
          {room_thread_failed()}
        </p>
        <button
          type="button"
          class="btn btn-sm preset-outlined-surface-500 self-start"
          onclick={() => void onRetry()}
        >
          {common_retry()}
        </button>
      {:else if lines.length === 0}
        <p class="text-sm opacity-60">{room_thread_empty()}</p>
      {:else}
        {#each lines as line (line.seq)}
          {@const pending = roomLinePending(line)}
          {@const quotedSeq = line.quote}
          {@const q = quotes.get(line.seq)}
          {@const chips = mentionChipsOf(
            line.mentions,
            memberById,
            selfId,
            rosterLoaded,
          )}
          {@const href = profileHref(
            line.senderKind,
            line.senderHandle,
            line.senderTagmaId,
          )}
          {@const rowLabel = roomMessageLabel(line, {
            time: formatStamp(line.createdAt, ROW_TIME),
            authorLabel: memberById.get(line.senderId)?.label,
          })}
          {#snippet extraActions()}
            {#if quotableLine(line)}
              <QuoteButton onclick={() => armQuote(line)} />
            {/if}
          {/snippet}
          <div
            use:reveal.trackRow={line.seq}
            tabindex="-1"
            class="group flex flex-col {line.mine
              ? 'items-end'
              : 'items-start'} {revealMark(reveal.highlightId, line.seq)}"
          >
            <span id={roomThreadRowAnchorId(line.seq)} class="sr-only">
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
                text={line.text}
                markdown={line.senderKind === "agent"}
                mine={line.mine}
                {pending}
                noActionRow={pending}
                pin={togglePin}
                {extraActions}
                failed={line.failed === true}
                quote={q}
                onQuoteClick={quotedSeq !== undefined &&
                drawnSeqs.has(quotedSeq)
                  ? () => jumpToQuoted(quotedSeq)
                  : undefined}
                quoteDescribedBy={quotedSeq !== undefined &&
                drawnSeqs.has(quotedSeq)
                  ? roomThreadRowAnchorId(quotedSeq)
                  : undefined}
                mentions={chips}
              />
            </MessageRow>
            {#if line.failed === true}
              <div class="flex items-center gap-2">
                {#if onResend !== undefined}
                  <button
                    type="button"
                    class="text-xs text-error-500 dark:text-error-400 hover:underline"
                    onclick={() => onResend(line)}>{common_retry()}</button
                  >
                {/if}
                <DeleteFailedButton onConfirm={() => discard(line.seq)} />
              </div>
            {/if}
          </div>
        {/each}
        {#if more === "loading"}
          <p class="text-xs opacity-60 text-center">
            {walking ? room_thread_walk_progress() : room_thread_more_loading()}
          </p>
        {:else if more === "failed"}
          <div class="flex items-center justify-center gap-2">
            <p class="text-xs text-error-500 dark:text-error-400">
              {room_thread_more_failed()}
            </p>
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
              onclick={() => void onRetryMore()}
            >
              {common_retry()}
            </button>
          </div>
        {:else if more === "stopped"}
          <div class="flex items-center justify-center gap-2">
            <p class="text-xs opacity-60">{room_thread_walk_stopped()}</p>
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
              onclick={() => void onContinueWalk()}
            >
              {room_thread_walk_continue()}
            </button>
          </div>
        {:else if more === "missing"}
          <p class="text-xs opacity-60 text-center">
            {room_thread_walk_missing()}
          </p>
        {:else if more === "unreachable"}
          <p class="text-xs opacity-60 text-center">
            {room_thread_more_unreachable()}
          </p>
        {/if}
        {#if hasMore && more === "idle"}
          <!-- Pager sentinel: zero height and inert to layout; the observer
           reports zero-area targets entering the root fine. -->
          <div
            bind:this={sentinel}
            class="h-0 w-full shrink-0"
            aria-hidden="true"
          ></div>
        {/if}
      {/if}
    </div>
  </div>
  {#if !scroll.follow}
    <!-- Why the raise: see SCROLL_PILL_RAISED. -->
    <div class={SCROLL_PILL_RAISED}>
      <ScrollToBottomButton
        missed={scroll.missed}
        onclick={() => void returnToNewest()}
      />
    </div>
  {/if}
</div>

{#snippet quoteBar()}
  {#if armedQuote !== undefined}
    <QuoteBar
      author={armedQuote.senderHandle}
      preview={previewOf(armedQuote)}
      onCancel={cancelArm}
    />
  {/if}
{/snippet}
<Composer
  bind:this={composerRef}
  {composer}
  disabled={loading || failed}
  pendingCount={0}
  disabledNotice={room_unavailable()}
  {quoteBar}
  onCancelQuote={cancelArm}
  onResized={() => scroll.reanchorIfFollowing()}
/>
