<script lang="ts">
  // The shared conversation render body used by every chat page: a scrollable
  // transcript (date/time markers, role bubbles, sending pulse, copy affordance,
  // inline turn-error) plus the composer, with stick-to-tail auto-scroll owned
  // internally. Page-specific chrome (status header, transport-offline banner,
  // no-channel empty state) stays in the page, which composes this as siblings
  // inside its own flex column.
  import Composer from "./Composer.svelte";
  import MessageBubble from "./MessageBubble.svelte";
  import MessageRow from "./MessageRow.svelte";
  import QuoteButton from "./QuoteButton.svelte";
  import ScrollToBottomButton from "./ScrollToBottomButton.svelte";
  import { RotateCcw } from "@lucide/svelte";
  import {
    createAutoScroll,
    createScrollAnchor,
    createTogglePin,
  } from "../lib/transcript.svelte.ts";
  import { timelineMarkers } from "../lib/channel/timeline.ts";
  import { createReveal, revealMark } from "../lib/reveal.svelte.ts";
  import { authorName, jumpAnnouncement } from "../lib/rowSpeech.ts";
  import { formatStamp, timezoneSetting } from "../lib/time/stamp.svelte.ts";
  import { ROW_TIME } from "../lib/time/stamp.ts";
  import {
    chat_send_to_start,
    chat_retry_send_aria,
    chat_error_profile_set_unusable,
    room_sending_aria,
    chat_message_label,
    chat_message_label_quoting,
    chat_message_label_quoting_timed,
    chat_message_label_timed,
    sender_agent_aria,
    sender_user_aria,
  } from "../paraglide/messages.js";
  import type {
    ConversationLine,
    ConversationTranscript,
  } from "../lib/transcript.ts";
  import type { ComposerModel } from "../lib/composer.svelte.ts";
  import type { Snippet } from "svelte";
  import type { QuoteView } from "../lib/quote.ts";
  import type { FileAttachment } from "@kallipai/kallipai-lesche-client";

  let {
    lines,
    status,
    error,
    composer,
    disabled,
    pendingCount,
    onRetry,
    notice,
    loadOlder,
    hasMoreOlder,
    loadingOlder,
    downloadAttachment,
    fileButton,
    attachmentBar,
    sessionKey,
    quoteBar,
    onCancelQuote,
    onQuote,
    resolveQuote,
    onQuoteRetry,
  }: {
    lines: ConversationLine[];
    status: ConversationTranscript["status"];
    error?: string;
    /** Optional: omitted by a read-only transcript consumer (the direct-
     * session page), which renders no composer at all. */
    composer?: ComposerModel;
    disabled: boolean;
    pendingCount: number;
    /** Optional retry affordance for failed lines: present = failed
     *  lines render an inline retry button wired to the page's retry handler. */
    onRetry?: (localId: number) => void;
    /** Optional lazy-window pager: when present, a sentinel above the first
     *  line fires it as it enters the viewport (scroll-up paging). All three
     *  props default off so non-windowed callers (online relay pages) keep
     *  today's behavior verbatim. */
    loadOlder?: () => void | Promise<void>;
    /** False once an older page came back empty (stops arming the sentinel). */
    hasMoreOlder?: boolean;
    /** True while a page is in flight (single-flight lives in the store). */
    loadingOlder?: boolean;
    /** Optional attachment bar snippet, placed under the composer's input
     *  card (page-owned items/handlers; forwarded verbatim). */
    attachmentBar?: Snippet;
    /** Optional file-attach affordance, forwarded to the composer verbatim
     *  (omitted = the button never renders). */
    fileButton?: { onFilesPicked: (files: File[]) => void };
    /** The page-supplied download I/O for message file cards, forwarded
     *  verbatim (omitted = the cards render without a download button). */
    downloadAttachment?: (attachment: FileAttachment) => Promise<void>;
    /** Identity of the conversation feeding `lines` (the route param). A
     *  change resets the auto-scroll controller, any quote reveal mark in
     *  flight, and the up-scroll anchor's saved session: SvelteKit reuses
     *  this component across param changes, and the new conversation must
     *  not inherit the old one's scroll position, missed count or pending
     *  reveal. Omitted = never reset (static callers). */
    sessionKey?: string;

    /** Optional page-supplied notice rendered inside the scrollable transcript
     *  (after the inline error), so it scrolls with the messages. Used for the
     *  online transport-offline banner. */
    notice?: Snippet;
    /** The composer's pending-quote strip, forwarded verbatim (page-owned
     *  state; omitted = the composer places nothing above the input). */
    quoteBar?: Snippet;
    /** Escape in the composer drops the pending quote. */
    onCancelQuote?: () => void;
    /** The quote entry point: the button a bubble's action row offers for a
     *  confirmed line. Present, each quotable row draws it; absent, no row
     *  renders a quote control and older callers are unchanged. */
    onQuote?: (line: ConversationLine) => void;
    /** Resolve a line's quote id against what the page knows (the loaded
     *  transcript, or a peeked outcome). Called per render, so the page's
     *  state drives the block; undefined means "not resolved yet" and the
     *  block renders nothing rather than a guess. */
    resolveQuote?: (id: number) => QuoteView | undefined;
    /** Re-attempt resolving a quote the page could not read. */
    onQuoteRetry?: (id: number) => void;
  } = $props();

  // busy drives the empty-state gate (only show "send a message" when idle and
  // empty); the inline error line renders when status === "error".
  const busy = $derived(status === "busy");
  // Per-line date divider / time label: a new group on day change or a >5min
  // gap; otherwise consecutive lines share the previous group's timestamp.
  const markers = $derived(
    timelineMarkers(lines, { timezone: timezoneSetting.value }),
  );

  // Stick to the tail as lines arrive; stop once the user scrolls up to read.
  const scroll = createAutoScroll();

  // Manual anchor for top prepends; createScrollAnchor explains why the
  // browser's own anchor is not relied on. The snapshot carries the session
  // it was taken in: a page still in flight when the session moves can land
  // later, and its delta belongs to the transcript that asked for it, never
  // to the one that replaced it.
  const anchor = createScrollAnchor(
    () => scroll.viewport,
    () => sessionKey,
  );
  // Session-key reset must be declared BEFORE the stick effect: effects in
  // the same flush run in declaration order, so a param change resets the
  // controllers before stick() observes the new conversation's lines. The
  // paging snapshot goes with them: it measures the transcript that is
  // leaving. A pending quote is not reset here: armedNow compares the record's
  // owner with the live conversation, so a swap makes it unreadable. Reading it
  // again is only possible while no controller call lands in between, because
  // own() drops the record on the next call made from another conversation.
  // The reveal's sentence drops from this effect's cleanup: destroy() reads
  // back the spoken slot it wrote to flip it, and a tracked body would
  // register that read and dirty itself; a cleanup has no reaction to record.
  $effect(() => {
    void sessionKey;
    scroll.reset();
    anchor.drop();
    return () => reveal.destroy();
  });
  $effect(() => {
    void lines.length;
    scroll.stick(lines.length, lines.at(-1)?.historyId);
  });
  // Layout-driven resizes (composer auto-grow, window resize) shrink the
  // container without a scroll event; while following, re-anchor to the
  // tail so a send never visually drags it out of view.
  $effect(() => {
    const vp = scroll.viewport;
    if (!vp) return;
    return scroll.observe(vp);
  });

  // One scroll-pin controller for the whole transcript (a single active
  // ResizeObserver across all bubbles); each <MessageBubble> hands its box +
  // action row to it on a raw toggle.
  const togglePin = createTogglePin(() => scroll.viewport);

  // --- quoting (optional; armed only when onQuote is passed) ---

  // A revealed quote target stays marked for a moment, so the reader who
  // jumped here sees WHICH message the quote pointed at. The row table, the
  // scroll and the mark are one controller, shared with the room transcript.
  const reveal = createReveal({
    scope: () => sessionKey,
    // A jump moves focus without a word, so the controller speaks the row
    // it landed on: the same author chain and the same time shape.
    describe: (id) => {
      const line = lines.find((row) => row.historyId === id);
      if (line === undefined) return undefined;
      const time =
        line.createdAt === undefined
          ? undefined
          : formatStamp(line.createdAt, ROW_TIME);
      return jumpAnnouncement(messageAuthor(line), time);
    },
  });

  // Only a message that exists on the tagma is quotable: an optimistic line
  // still sending (or failed) has no id the other side could resolve, and a
  // system line has no author at all.
  function quotable(line: ConversationLine): boolean {
    return (
      onQuote !== undefined &&
      line.historyId > 0 &&
      line.status !== "sending" &&
      line.status !== "failed"
    );
  }
  // A flash timer outliving the transcript would touch torn-down state: the
  // final teardown runs the reveal's own cleanup -- its mark and sentence
  // with its timer -- and a session swap drops the sentence from its own
  // effect's cleanup, its body resetting the mark.
  $effect(() => () => {
    reveal.destroy();
  });

  // Who wrote a line, as spoken: the sender's handle when the row carries
  // one, else the name its role stands under. Shared by the row's label and
  // by the jump announcement, so both name a row the same way.
  function messageAuthor(line: ConversationLine): string {
    return authorName(
      line.sender?.handle,
      line.role === "user" ? sender_user_aria() : sender_agent_aria(),
    );
  }

  // A log region announces text only, so who wrote a line, when, and whether
  // it answers a quote have to be spoken: the layout otherwise carries them
  // for sighted readers alone. The body itself follows as the row's text.
  function messageLabel(
    line: ConversationLine,
    time: string | undefined,
  ): string {
    const author = messageAuthor(line);
    const quoting = line.quote !== undefined;
    if (time === undefined) {
      return quoting
        ? chat_message_label_quoting({ author })
        : chat_message_label({ author });
    }
    return quoting
      ? chat_message_label_quoting_timed({ author, time })
      : chat_message_label_timed({ author, time });
  }

  // --- lazy-window scroll paging (optional; armed only when loadOlder is passed) ---

  // The zero-height sentinel above the first line. An IntersectionObserver
  // (not a scroll handler) fires the pager, so the auto-scroll controller's
  // onScroll contract stays untouched; a full prepend pushes the sentinel
  // back out of view, which naturally stops the arm loop.
  let sentinel: HTMLDivElement | undefined = $state();
  $effect(() => {
    const root = loadOlder ? scroll.viewport : undefined;
    if (!loadOlder || !sentinel) return;
    const io = new IntersectionObserver(
      (entries) => {
        if (!entries[0]?.isIntersecting) return;
        if (hasMoreOlder === false || loadingOlder) return;
        anchor.capture(lines.length);
        void loadOlder();
      },
      // The scroll container as IO root: with the implicit viewport root
      // the observer skips recalculating on inner-container scrolling
      // (observed live in e2e), and the sentinel never fires.
      { root },
    );
    io.observe(sentinel);
    return () => io.disconnect();
  });

  // Apply a landed page's delta, then forget it: a no-op page (the store's
  // zero-add outcome) never changes lines.length, so the release below drops
  // a snapshot whose page settled without adding anything.
  $effect(() => {
    void lines.length;
    anchor.consume(lines.length);
  });
  $effect(() => {
    anchor.release(lines.length, loadingOlder === true);
  });
</script>

<div class="relative flex-1 min-h-0 flex">
  <!-- A jump moves focus without a word, so the row it landed on is spoken
       here. Two slots, written in turn: two jumps that land on rows named the
       same are two separate events, and re-writing one node with the same
       text says nothing at all. -->
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
    <div
      class="mx-auto w-full max-w-[80rem] p-4 flex flex-col gap-3"
      role="log"
    >
      {#if loadOlder}
        <!-- Pager sentinel: zero-height and inert to layout; the observer
           reports zero-area targets entering the root fine. -->
        <div
          bind:this={sentinel}
          class="h-0 w-full shrink-0"
          aria-hidden="true"
        ></div>
      {/if}
      {#if lines.length === 0 && !busy && composer}
        <p class="text-sm opacity-60 text-center mt-8">
          {chat_send_to_start()}
        </p>
      {/if}
      {#each lines as line, i (line.historyId)}
        {@const m = markers[i]}
        {@const quotedId = line.quote}
        {@const quoted =
          quotedId !== undefined ? resolveQuote?.(quotedId) : undefined}
        {@const quoteTarget = quoted?.jumpable === true ? quotedId : undefined}
        {@const quoteJump =
          quoteTarget !== undefined
            ? () => reveal.jump(quoteTarget)
            : undefined}
        {@const quoteRetry =
          quotedId !== undefined && onQuoteRetry
            ? () => onQuoteRetry?.(quotedId)
            : undefined}
        {@const quotedLabelId =
          quoteTarget !== undefined ? `msg-a11y-${quoteTarget}` : undefined}
        {#snippet extraActions()}
          {#if quotable(line)}
            <QuoteButton onclick={() => onQuote?.(line)} />
          {/if}
        {/snippet}
        {#if m?.dateDivider}
          <div
            class="self-center text-xs opacity-50 my-2 text-center max-w-[80%]"
          >
            {m.dateDivider}{#if m.timeLabel}
              <span class="opacity-70">· {m.timeLabel}</span>{/if}
          </div>
        {:else if m?.timeLabel}
          <div class="self-center text-xs opacity-50 mt-2 text-center">
            {m.timeLabel}
          </div>
        {/if}
        {#if line.role === "system"}
          <p
            class="text-xs opacity-60 text-center whitespace-pre-wrap break-words"
          >
            {line.text}
          </p>
        {:else}
          <div
            use:reveal.trackRow={line.historyId}
            tabindex="-1"
            class="group flex flex-col {line.role === 'user'
              ? 'items-end'
              : 'items-start'} {revealMark(reveal.highlightId, line.historyId)}"
          >
            <span id="msg-a11y-{line.historyId}" class="sr-only">
              {messageLabel(line, m?.timeLabel)}
            </span>
            {#if line.role !== "user" && line.sender && (i === 0 || lines[i - 1]?.sender?.id !== line.sender.id)}
              <span class="text-xs opacity-50 px-1 mb-0.5"
                >{line.sender.handle}</span
              >
            {/if}
            <!-- The row is the bubble's only width cap and its alignment. A
                 non-bare line's actions ride in the bubble's own row under it,
                 on the sender's side without capping the bubble. -->
            <MessageRow mine={line.role === "user"}>
              {#if line.role === "user" && line.status === "failed"}
                <!-- Failed lines are transient: a bare bubble carries the
                   outline and the text; retry only where the page wires a
                   handler; no copy, raw toggle or quote rides here. -->
                <div class="flex flex-row items-center gap-1.5 self-end">
                  {#if onRetry}
                    <button
                      type="button"
                      class="shrink-0 size-7 rounded-full preset-outlined-error-500 hover:preset-filled-error-500 flex items-center justify-center cursor-pointer"
                      aria-label={chat_retry_send_aria()}
                      onclick={() => onRetry(line.historyId)}
                    >
                      <RotateCcw class="size-3.5" aria-hidden="true" />
                    </button>
                  {/if}
                  <MessageBubble
                    text={line.text}
                    mine
                    bare
                    failed
                    failureCopy={line.errorCode === "profile_set_unusable"
                      ? chat_error_profile_set_unusable()
                      : line.error}
                    attachment={line.attachment}
                    {downloadAttachment}
                    quote={quoted}
                    quoteDescribedBy={quotedLabelId}
                    onQuoteClick={quoteJump}
                    onQuoteRetry={quoteRetry}
                  />
                </div>
              {:else}
                <MessageBubble
                  {extraActions}
                  text={line.text}
                  markdown={line.role === "assistant"}
                  mine={line.role === "user"}
                  bare={line.role === "user" && line.status === "sending"}
                  pending={line.status === "sending"}
                  attachment={line.attachment}
                  {downloadAttachment}
                  quote={quoted}
                  quoteDescribedBy={quotedLabelId}
                  pin={togglePin}
                  onQuoteClick={quoteJump}
                  onQuoteRetry={quoteRetry}
                />
              {/if}
            </MessageRow>
            {#if line.role === "user" && line.status === "sending"}
              <span
                class="text-xs opacity-50 animate-pulse"
                aria-label={room_sending_aria()}>··</span
              >
            {/if}
          </div>
        {/if}
      {/each}
      {#if status === "error" && error}
        <p class="text-xs text-error-500 dark:text-error-400 text-center">
          {error}
        </p>
      {/if}
      {@render notice?.()}
    </div>
  </div>
  {#if !scroll.follow}
    <!-- z-10: a hosting page may append absolute siblings after this one;
      later siblings stack above by DOM order unless outranked. -->
    <div
      class="absolute inset-x-0 bottom-4 z-10 mx-auto flex max-w-[80rem] justify-end px-4 pointer-events-none"
    >
      <ScrollToBottomButton
        missed={scroll.missed}
        onclick={() => scroll.forceBottom()}
      />
    </div>
  {/if}
</div>
{#if composer}
  <Composer
    {composer}
    {disabled}
    {pendingCount}
    {attachmentBar}
    {fileButton}
    {quoteBar}
    {onCancelQuote}
    onSubmitted={() => scroll.forceBottom()}
    onResized={() => scroll.reanchorIfFollowing()}
  />
{/if}
