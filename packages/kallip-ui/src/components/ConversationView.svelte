<script lang="ts">
  // The shared conversation render body used by every chat page: a scrollable
  // transcript (date/time markers, role bubbles, sending pulse, copy affordance,
  // inline turn-error) plus the composer, with stick-to-tail auto-scroll owned
  // internally. Page-specific chrome (status header, transport-offline banner,
  // no-channel empty state) stays in the page, which composes this as siblings
  // inside its own flex column.
  import Composer from "./Composer.svelte";
  import MessageBubble from "./MessageBubble.svelte";
  import ScrollToBottomButton from "./ScrollToBottomButton.svelte";
  import {
    createAutoScroll,
    createTogglePin,
  } from "../lib/transcript.svelte.ts";
  import { timelineMarkers } from "../lib/channel/timeline.ts";
  import {
    chat_send_to_start,
    room_sending_aria,
  } from "../paraglide/messages.js";
  import type {
    ConversationLine,
    ConversationTranscript,
  } from "../lib/transcript.ts";
  import type { ComposerModel } from "../lib/composer.svelte.ts";
  import type { Snippet } from "svelte";
  import type { FileAttachment } from "@kallipai/kallip-lesche-client";

  let {
    lines,
    status,
    error,
    composer,
    disabled,
    pendingCount,
    notice,
    loadOlder,
    hasMoreOlder,
    loadingOlder,
    downloadAttachment,
    fileButton,
    attachmentBar,
    sessionKey,
  }: {
    lines: ConversationLine[];
    status: ConversationTranscript["status"];
    error?: string;
    /** Optional: omitted by a read-only transcript consumer (the direct-
     * session page), which renders no composer at all. */
    composer?: ComposerModel;
    disabled: boolean;
    pendingCount: number;
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
     *  change resets the auto-scroll controller: SvelteKit reuses this
     *  component across param changes, and the new conversation must not
     *  inherit the old one's scroll position or missed count. Omitted =
     *  never reset (static callers). */
    sessionKey?: string;

    /** Optional page-supplied notice rendered inside the scrollable transcript
     *  (after the inline error), so it scrolls with the messages. Used for the
     *  online transport-offline banner. */
    notice?: Snippet;
  } = $props();

  // busy drives the empty-state gate (only show "send a message" when idle and
  // empty); the inline error line renders when status === "error".
  const busy = $derived(status === "busy");
  // Per-line date divider / time label: a new group on day change or a >5min
  // gap; otherwise consecutive lines share the previous group's timestamp.
  const markers = $derived(timelineMarkers(lines));

  // Stick to the tail as lines arrive; stop once the user scrolls up to read.
  const scroll = createAutoScroll();
  // Session-key reset must be declared BEFORE the stick effect: effects in
  // the same flush run in declaration order, so a param change resets the
  // controller before stick() observes the new conversation's lines.
  $effect(() => {
    void sessionKey;
    scroll.reset();
  });
  $effect(() => {
    void lines.length;
    scroll.stick(lines.length, lines.at(-1)?.historyId);
  });

  // One scroll-pin controller for the whole transcript (a single active
  // ResizeObserver across all bubbles); each <MessageBubble> hands its box +
  // actions elements to it on a raw toggle.
  const togglePin = createTogglePin(() => scroll.viewport);

  // --- lazy-window scroll paging (optional; armed only when loadOlder is passed) ---

  // The zero-height sentinel above the first line. An IntersectionObserver
  // (not a scroll handler) fires the pager, so the auto-scroll controller's
  // onScroll contract stays untouched; a full prepend pushes the sentinel
  // back out of view, which naturally stops the arm loop.
  let sentinel: HTMLDivElement | undefined = $state();
  // Manual scroll anchor for top prepends: CSS overflow-anchor is UA- and
  // WebView-dependent (untestable across our matrix), so we snapshot
  // (scrollTop, scrollHeight) before the page lands and restore the
  // position by the height delta once the DOM has grown.
  let anchor: {
    scrollTop: number;
    scrollHeight: number;
    count: number;
  } | null = null;
  $effect(() => {
    const root = loadOlder ? scroll.viewport : undefined;
    if (!loadOlder || !sentinel) return;
    const io = new IntersectionObserver(
      (entries) => {
        if (!entries[0]?.isIntersecting) return;
        if (hasMoreOlder === false || loadingOlder) return;
        const vp = scroll.viewport;
        if (vp && !anchor) {
          anchor = {
            scrollTop: vp.scrollTop,
            scrollHeight: vp.scrollHeight,
            count: lines.length,
          };
        }
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

  // Apply (and consume) the anchor after the DOM settles. A no-op page (the
  // store's zero-add outcome) never changes lines.length, so the second
  // effect reclaims the stale anchor once loadingOlder falls.
  $effect(() => {
    void lines.length;
    const vp = scroll.viewport;
    if (anchor && vp && lines.length !== anchor.count) {
      const delta = vp.scrollHeight - anchor.scrollHeight;
      if (delta > 0) vp.scrollTop = anchor.scrollTop + delta;
      anchor = null;
    }
  });
  $effect(() => {
    if (!loadingOlder && anchor && lines.length === anchor.count) anchor = null;
  });
</script>

<div class="relative flex-1 min-h-0 flex">
  <div
    class="flex-1 min-h-0 overflow-auto"
    bind:this={scroll.viewport}
    onscroll={scroll.onScroll}
  >
    <div class="mx-auto w-full max-w-[80rem] p-4 flex flex-col gap-3">
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
            class="group flex flex-col {line.role === 'user'
              ? 'items-end'
              : 'items-start'}"
          >
            {#if line.role !== "user" && line.sender && (i === 0 || lines[i - 1]?.sender?.id !== line.sender.id)}
              <span class="text-xs opacity-50 px-1 mb-0.5"
                >{line.sender.handle}</span
              >
            {/if}
            <MessageBubble
              text={line.text}
              markdown={line.role === "assistant"}
              mine={line.role === "user"}
              bare={line.role === "user" && line.status === "sending"}
              pending={line.status === "sending"}
              attachment={line.attachment}
              {downloadAttachment}
              pin={togglePin}
            />
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
      class="absolute inset-x-0 bottom-4 z-10 flex justify-center pointer-events-none"
    >
      <ScrollToBottomButton
        missed={scroll.missed}
        onclick={() => scroll.forceBottom()}
      />
    </div>
  {/if}
</div>
{#if composer}
  <Composer {composer} {disabled} {pendingCount} {attachmentBar} {fileButton} />
{/if}
