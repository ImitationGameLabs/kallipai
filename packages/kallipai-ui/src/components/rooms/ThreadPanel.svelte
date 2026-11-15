<script lang="ts">
  // The reading surface for one discussion: the pages of it that have been
  // read, in order, with the same bubble a transcript row draws. It is a leaf:
  // the page decides which discussion is open, reads it a page at a time, and
  // walks down it when a quote names a reply this panel does not hold yet, so
  // this component never guesses a count or a reason.
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
  import { Send, X } from "@lucide/svelte";
  import ScrollToBottomButton from "../ScrollToBottomButton.svelte";
  import DeleteFailedButton from "../DeleteFailedButton.svelte";
  import MessageBubble from "../MessageBubble.svelte";
  import MessageRow from "../MessageRow.svelte";
  import SenderIdentity from "./SenderIdentity.svelte";
  import { TONAL_ICON_SURF } from "../../lib/classes.ts";
  import { createAutoScroll } from "../../lib/transcript.svelte.ts";
  import { revealMark } from "../../lib/reveal.svelte.ts";
  import { ROW_TIME } from "../../lib/time/stamp.ts";
  import { formatStamp } from "../../lib/time/stamp.svelte.ts";
  import {
    mentionChipsOf,
    profileHref,
    roomLinePending,
    roomMessageLabel,
    roomThreadRowAnchorId,
    type MentionTarget,
    type RoomLine,
  } from "../../lib/room-message.ts";
  import { roomQuoteBlocksByLine } from "../../lib/quote.ts";
  import { type ThreadMembership, type ThreadMore } from "../../lib/threads.ts";
  import {
    chat_send_failed,
    composer_placeholder,
    composer_send_aria,
  } from "../../paraglide/messages.js";
  import {
    room_thread_membership_mentioned,
    room_thread_membership_replied,
    room_thread_membership_started,
  } from "../../paraglide/messages.js";
  import {
    common_retry,
    room_thread_close_aria,
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
  } from "../../paraglide/messages.js";

  let {
    root,
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
    membership,
    onClose,
    onReply,
    onRetry,
    onRetryMore,
    onLoadMore,
    onContinueWalk,
    onReturnToNewest,
    onDiscard,
    trackRow,
    highlightId,
    onQuoteJump,
  }: {
    /** Which discussion this panel is reading. A change is a different
     *  discussion: the draft and the scroll belong to the one that left. */
    root: number;
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
    onClose: () => void;
    /** Send one reply into this discussion. Rejects on failure; the panel
     *  keeps the text and shows the shared send-failure line. */
    onReply: (text: string) => Promise<void>;
    /** Drop a failed reply. Answers whether it left; the panel keeps the
     *  focus, because the transcript behind it is not where the reader is. */
    onDiscard: (seq: number) => boolean;
    /** The page's row table for the panel's own rows, so a quote that points
     *  at a reply can land on it here. */
    trackRow: (node: HTMLElement, id: number) => { destroy(): void };
    /** The row last revealed by a quote jump, while its mark is on. */
    highlightId: number | null;
    /** Land on a row this panel draws. A quote of a room message outside
     *  the discussion has nowhere here to land, so it stays inert. */
    onQuoteJump?: (seq: number) => void;
    /** Why the reader is in this discussion, or null when the panel cannot
     *  say: no reason it read, or a discussion it could not read whole. */
    membership: ThreadMembership | null;
  } = $props();

  const quotes = $derived(roomQuoteBlocksByLine(lines, roomLines));
  // The seqs this panel draws: what a quote can be described by or jump to.
  const drawnSeqs = $derived(new Set(lines.map((line) => line.seq)));

  const membershipCopy = $derived.by(() => {
    if (membership === "started") return room_thread_membership_started();
    if (membership === "replied") return room_thread_membership_replied();
    if (membership === "mentioned") return room_thread_membership_mentioned();
    return null;
  });
  // The reply box: this panel's own draft, cleared only once the send
  // answered, so a failed reply keeps the words the reader typed. A different
  // discussion starts empty.
  let draft = $state("");
  let busy = $state(false);
  let sendFailed = $state(false);
  let replyEl = $state<HTMLTextAreaElement | null>(null);
  // Which stretch of input the panel is in. A reset -- another discussion or
  // another room -- starts a new one, so an answer that arrives after it is
  // about words the reader can no longer see, even when the discussion
  // number happens to match again.
  let epoch = 0;

  // A reader paging forward through a discussion is not reading a chat tail:
  // a page that lands below them must not pull them to its bottom. Following
  // is for the three places that ask for it -- the pill, an own send, and a
  // walk -- and each of those moves the viewport itself.
  const scroll = createAutoScroll({ stickOnAppend: false });
  // Set while the panel is waiting for its first page, so the page that lands
  // puts the reader at the discussion's start instead of its tail. A plain
  // variable, not state: it steers one scroll, and nothing draws from it.
  let landAtTop = false;
  let sentinel = $state<HTMLDivElement | null>(null);

  // A different discussion brings its own start: the draft goes with the one
  // that left, and the reading position restarts at the top, on the page the
  // root sits on.
  $effect(() => {
    void root;
    epoch += 1;
    draft = "";
    busy = false;
    sendFailed = false;
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

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const text = draft.trim();
    if (text === "" || busy) return;
    const openedEpoch = epoch;
    busy = true;
    sendFailed = false;
    try {
      await onReply(text);
      if (openedEpoch !== epoch) return;
      draft = "";
      // The reply is the discussion's newest row: the page brought the
      // panel down to it, so the reader sees what they just wrote.
      scroll.forceBottom();
    } catch (e) {
      console.error("[room thread] send failed:", e);
      if (openedEpoch !== epoch) return;
      sendFailed = true;
    } finally {
      if (openedEpoch === epoch) busy = false;
    }
  }

  /** Enter sends; Shift+Enter keeps the line break the reader asked for. */
  function onKeydown(event: KeyboardEvent): void {
    if (event.key !== "Enter" || event.shiftKey || event.isComposing) return;
    event.preventDefault();
    void submit(new SubmitEvent("submit"));
  }

  /** A discarded reply takes its own row with it; the reply box is where the
   *  reader continues, and the failed trigger never receives focus again. */
  function discard(seq: number): boolean {
    const left = onDiscard(seq);
    if (left) replyEl?.focus({ preventScroll: true });
    return left;
  }
</script>

<header
  class="px-3 py-2 border-b border-surface-200-800 flex items-start justify-between gap-2 shrink-0"
>
  <div class="min-w-0 flex flex-col gap-0.5">
    <p class="text-sm font-semibold truncate">{room_thread_panel_title()}</p>
    {#if membershipCopy}
      <p class="text-xs opacity-60">{membershipCopy}</p>
    {/if}
  </div>
  <button
    type="button"
    class="size-7 {TONAL_ICON_SURF}"
    aria-label={room_thread_close_aria()}
    onclick={onClose}
  >
    <X class="size-4" />
  </button>
</header>

<div class="relative flex-1 min-h-0 flex flex-col">
  <div
    class="flex-1 min-h-0 overflow-auto p-3 flex flex-col gap-3"
    bind:this={scroll.viewport}
    onscroll={scroll.onScroll}
    role="group"
    aria-label={room_thread_panel_title()}
  >
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
        <div
          use:trackRow={line.seq}
          tabindex="-1"
          class="flex flex-col {line.mine
            ? 'items-end'
            : 'items-start'} {revealMark(highlightId, line.seq)}"
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
              bare={pending}
              failed={line.failed === true}
              quote={q}
              onQuoteClick={quotedSeq !== undefined && drawnSeqs.has(quotedSeq)
                ? () => onQuoteJump?.(quotedSeq)
                : undefined}
              quoteDescribedBy={quotedSeq !== undefined &&
              drawnSeqs.has(quotedSeq)
                ? roomThreadRowAnchorId(quotedSeq)
                : undefined}
              mentions={chips}
            />
          </MessageRow>
          {#if line.failed === true}
            <DeleteFailedButton onConfirm={() => discard(line.seq)} />
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
  {#if !scroll.follow}
    <!-- z-10: a hosting page may append absolute siblings after this one;
      later siblings stack above by DOM order unless outranked. -->
    <div
      class="absolute inset-x-0 bottom-3 z-10 mx-auto flex justify-end px-3 pointer-events-none"
    >
      <ScrollToBottomButton
        missed={scroll.missed}
        onclick={() => void returnToNewest()}
      />
    </div>
  {/if}
</div>

<form
  class="p-3 border-t border-surface-200-800 flex items-end gap-2 shrink-0"
  onsubmit={submit}
>
  <textarea
    bind:this={replyEl}
    class="input flex-1 resize-none"
    rows="2"
    bind:value={draft}
    placeholder={composer_placeholder()}
    aria-label={composer_placeholder()}
    onkeydown={onKeydown}></textarea>
  <button
    type="submit"
    class="btn preset-filled-primary-500 text-on-primary-500 px-3 py-2"
    disabled={busy || draft.trim() === ""}
    aria-label={composer_send_aria()}
  >
    <Send class="size-4" />
  </button>
</form>
{#if sendFailed}
  <p class="px-3 pb-2 text-xs text-error-500 dark:text-error-400">
    {chat_send_failed()}
  </p>
{/if}
