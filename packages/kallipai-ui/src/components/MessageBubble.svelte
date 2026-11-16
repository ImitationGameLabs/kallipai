<script lang="ts">
  // A single chat message bubble: the content box (rendered markdown, raw source, or plain
  // text) plus the hover-revealed copy + raw-toggle affordances. Shared by every chat
  // surface -- the bilateral ConversationView and the multi-member RoomConversationPage --
  // so agent replies render identically everywhere and the markup lives in one place.
  //
  // A presentation LEAF: it owns only its per-bubble raw-source flag + element
  // refs. The surrounding `.group`, alignment (items-end/items-start), sender
  // header, sending pulse, and scroll-pinning all stay in the parent column.
  // Raw-toggling hands the box + action row to the parent-supplied `pin`
  // (a createTogglePin controller), because keeping the clicked control under
  // the cursor is a viewport-level concern that must share ONE active observer
  // across the whole transcript.
  //
  // The action row under the bubble is the bubble's own, plus whatever the
  // surface hands in: the bubble never learns what that action does.
  import { Download, File } from "@lucide/svelte";
  import Markdown from "./Markdown.svelte";
  import CopyButton from "./CopyButton.svelte";
  import RawToggleButton from "./RawToggleButton.svelte";
  import QuoteBlock from "./QuoteBlock.svelte";
  import MentionChips from "./MentionChips.svelte";
  import type { Snippet } from "svelte";
  import type { TogglePin } from "../lib/transcript.svelte.ts";
  import { BUBBLE_ENTRY_MARK, ROW_LANDED } from "../lib/classes.ts";
  import { bubbleActivates } from "../lib/bubble_click.ts";
  import { selectionAtClickOf } from "../lib/selection.ts";
  import type { QuoteView } from "../lib/quote.ts";
  import type { MentionChip } from "../lib/room-message.ts";
  import type { FileAttachment } from "@kallipai/kallipai-lesche-client";
  import {
    chat_file_download_aria,
    chat_file_unreadable,
    chat_file_size_bytes,
  } from "../paraglide/messages.js";

  let {
    text,
    markdown = false,
    mine = false,
    noActionRow = false,
    pending = false,
    failed = false,
    failureCopy,
    pin,
    attachment,
    downloadAttachment,
    mentions,
    quote,
    quoteDescribedBy,
    onQuoteClick,
    onQuoteRetry,
    extraActions,
    footer,
    entry,
  }: {
    text: string;
    /** Render the source as markdown (agent/assistant replies) with a raw-text toggle.
     *  Plain-text lines (human/user) pass false. */
    markdown?: boolean;
    /** Own message: right-aligned column + filled-primary fill (the column lives in the
     *  parent; this only picks the fill). */
    mine?: boolean;
    /** Draw no action row under the bubble: the copy, the raw toggle, and
     *  the surface's own action. Each surface that omits it has its own
     *  reason: a line still sending shows the pulse instead, a failed one
     *  carries its retry outside the box, and a discussion bubble keeps its
     *  actions in the panel. */
    noActionRow?: boolean;
    /** Dim the box (optimistic/unconfirmed lines). */
    pending?: boolean;
    /** Failed line: error outline on the box. */
    failed?: boolean;
    /** Optional inline failure text under the content (the retry hint). */
    failureCopy?: string;
    /** Parent's scroll-pin controller, invoked on a raw toggle. Optional so the bubble can
     *  render outside a scroll context (no pinning). */
    pin?: TogglePin;
    /** Optional file reference riding the message: present, the bubble
     *  renders a file card above the text. Absent, nothing changes. */
    attachment?: FileAttachment;
    /** The page-supplied download I/O (files get -> blob -> anchor). The
     *  card stays a dumb renderer; without it the card has no button. */
    downloadAttachment?: (attachment: FileAttachment) => Promise<void>;
    /** The members this line mentions, already resolved against the room
     *  roster by the page. Absent (a plain message, and always on the 1:1
     *  surface, which has no mentions) -> nothing renders below the body. */
    mentions?: readonly MentionChip[];
    /** The quoted message this line answers, already resolved against the
     *  transcript by the quotes controller. Absent for a plain message. */
    quote?: QuoteView;
    /** Id of the accessible label of the quoted row, when it is loaded: the
     *  block then points its description there instead of repeating the
     *  quoted text to a screen reader. */
    quoteDescribedBy?: string;
    /** Scroll to the quoted line (found quotes only). */
    onQuoteClick?: () => void;
    /** Re-attempt loading a quoted message that failed to resolve. */
    onQuoteRetry?: () => void;
    /** The surface's own action for this message, rendered in the action
     *  row under the bubble, before the copy and raw toggles. Absent, the
     *  row holds the bubble's own controls alone. */
    extraActions?: Snippet;
    /** Content the surface draws inside the box, under everything else: a
     *  discussion row states there how many replies it holds. Absent, the box
     *  ends where it always did. */
    footer?: Snippet;
    /** The room's entry for a discussion: a click on the box body opens it,
     *  and the box wears the ring that says an entry sits here -- at rest, so
     *  a reader sees where a discussion starts before touching it, and the
     *  hover thickens that ring instead of recolouring the box. One prop
     *  carries both halves because they never come apart: a ring without the
     *  click advertises nothing, a click without the ring is a target no one
     *  can see. The box itself stays a target, not a control. */
    entry?: () => void;
  } = $props();

  // Per-bubble raw-source view: ephemeral, resets when the bubble unmounts.
  let raw = $state(false);
  let box: HTMLDivElement | undefined = $state();
  let actionRow: HTMLDivElement | undefined = $state();

  function toggleRaw(): void {
    raw = !raw;
    if (box && actionRow) pin?.(box, actionRow);
  }

  // A click on the box body belongs to the surface, when it asked for one.
  // Two things inside keep their own click: an inner control (the quote
  // block, the download button, a link) and a drag that selected text. The
  // keyboard path is the surface's own control inside `footer`, which is why
  // the box is not a control itself and does not pretend to be one.
  // The gate only counts a control the box itself holds: a click that
  // walked up to a control above the bubble is the body's.
  function activate(event: MouseEvent): void {
    if (entry === undefined) return;
    const body = event.target instanceof Element ? event.target : null;
    const selectionAtClick = selectionAtClickOf(
      globalThis.getSelection(),
      box ?? null,
    );
    if (bubbleActivates(body, selectionAtClick, box)) entry();
  }

  // A failed download renders inline (unavailable) -- never a dialog or a
  // blank bubble; the flag is ephemeral per bubble.
  let downloadFailed = $state(false);

  async function download(): Promise<void> {
    if (!attachment || !downloadAttachment) return;
    downloadFailed = false;
    try {
      await downloadAttachment(attachment);
    } catch {
      downloadFailed = true;
    }
  }
</script>

<!-- The box body is a target for the surface, not a control: the keyboard
     path is the button the surface renders inside `footer`. -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
  bind:this={box}
  data-landing-target
  onclick={entry === undefined ? undefined : activate}
  class="max-w-full min-w-0 rounded-base px-3 py-2 text-sm {ROW_LANDED} {mine
    ? 'preset-filled-primary-100-900'
    : 'preset-tonal-surface'} {!markdown
    ? 'whitespace-pre-wrap break-words'
    : ''} {pending ? 'opacity-60' : ''} {failed
    ? 'ring-1 ring-error-500 dark:ring-error-400'
    : ''} {entry ? BUBBLE_ENTRY_MARK : ''}"
>
  {#if attachment}
    <div
      class="mb-1 flex items-center gap-2 rounded-lg border border-black/10 bg-black/5 px-2 py-1.5 dark:border-white/20 dark:bg-white/10"
    >
      <File class="size-4 shrink-0 opacity-70" aria-hidden="true" />
      <span class="min-w-0 flex-1 truncate">{attachment.name}</span>
      <span class="shrink-0 text-xs opacity-60"
        >{chat_file_size_bytes({ size: attachment.size })}</span
      >
      {#if downloadAttachment}
        <button
          type="button"
          class="shrink-0 opacity-70 hover:opacity-100 cursor-pointer"
          aria-label={chat_file_download_aria({ name: attachment.name })}
          onclick={download}
        >
          <Download class="size-4" aria-hidden="true" />
        </button>
      {/if}
    </div>
    {#if downloadFailed}
      <p class="text-xs text-error-500 dark:text-error-400">
        {chat_file_unreadable()}
      </p>
    {/if}
  {/if}
  {#if quote}
    <QuoteBlock
      view={quote}
      describedBy={quoteDescribedBy}
      onClick={onQuoteClick}
      onRetry={onQuoteRetry}
    />
  {/if}
  {#if markdown}
    {#if raw}
      <div class="min-w-0 whitespace-pre-wrap break-words font-mono text-xs">
        {text}
      </div>
    {:else}
      <Markdown source={text} />
    {/if}
  {:else}
    {text}
  {/if}
  {#if mentions && mentions.length > 0}
    <MentionChips chips={mentions} />
  {/if}
  {#if failureCopy}
    <p class="mt-1 text-xs text-error-500 dark:text-error-400">
      {failureCopy}
    </p>
  {/if}
  {@render footer?.()}
</div>
{#if !noActionRow}
  <!-- Under the bubble: natural width, so the row's alignment puts it on the
       same side as the bubble, and the row costs the bubble no width. The
       surface's own action leads; a line that omits the row draws none here. -->
  <div bind:this={actionRow} class="flex items-center gap-1">
    {@render extraActions?.()}
    <CopyButton getText={() => text} />
    {#if markdown}
      <RawToggleButton pressed={raw} onclick={toggleRaw} />
    {/if}
  </div>
{/if}
