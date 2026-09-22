<script lang="ts">
  // A single chat message bubble: the content box (rendered markdown, raw source, or plain
  // text) plus the hover-revealed copy + raw-toggle affordances. Shared by every chat
  // surface -- the bilateral ConversationView and the multi-member RoomConversationPage --
  // so agent replies render identically everywhere and the markup lives in one place.
  //
  // A presentation LEAF: it owns only its per-bubble raw-source flag + element refs. The
  // surrounding `.group`, alignment (items-end/items-start), sender header, sending pulse,
  // and scroll-pinning all stay in the parent column. Raw-toggling hands the box + actions
  // elements to the parent-supplied `pin` (a createTogglePin controller), because keeping
  // the clicked control under the cursor is a viewport-level concern that must share ONE
  // active observer across the whole transcript.
  import { Download, File } from "@lucide/svelte";
  import Markdown from "./Markdown.svelte";
  import CopyButton from "./CopyButton.svelte";
  import RawToggleButton from "./RawToggleButton.svelte";
  import type { TogglePin } from "../lib/transcript.svelte.ts";
  import type { FileAttachment } from "@kallipai/kallip-lesche-client";
  import {
    chat_file_download_aria,
    chat_file_unreadable,
    chat_file_size_bytes,
  } from "../paraglide/messages.js";

  let {
    text,
    markdown = false,
    mine = false,
    bare = false,
    pending = false,
    failed = false,
    failureCopy,
    pin,
    attachment,
    downloadAttachment,
  }: {
    text: string;
    /** Render the source as markdown (agent/assistant replies) with a raw-text toggle.
     *  Plain-text lines (human/user) pass false. */
    markdown?: boolean;
    /** Own message: right-aligned column + filled-primary fill (the column lives in the
     *  parent; this only picks the fill). */
    mine?: boolean;
    /** Omit the copy/raw-toggle row (e.g. an in-flight optimistic own message, where the
     *  sending pulse replaces the actions). */
    bare?: boolean;
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
  } = $props();

  // Per-bubble raw-source view: ephemeral, resets when the bubble unmounts.
  let raw = $state(false);
  let box: HTMLDivElement | undefined = $state();
  let actions: HTMLDivElement | undefined = $state();

  function toggleRaw(): void {
    raw = !raw;
    if (box && actions) pin?.(box, actions);
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

<div
  bind:this={box}
  class="max-w-[80%] min-w-0 rounded-base px-3 py-2 text-sm {mine
    ? 'preset-filled-primary-100-900'
    : 'preset-tonal-surface'} {!markdown
    ? 'whitespace-pre-wrap break-words'
    : ''} {pending ? 'opacity-60' : ''} {failed
    ? 'ring-1 ring-error-500 dark:ring-error-400'
    : ''}"
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
  {#if failureCopy}
    <p class="mt-1 text-xs text-error-500 dark:text-error-400">{failureCopy}</p>
  {/if}
</div>
{#if !bare}
  <div bind:this={actions} class="flex items-center gap-1">
    <CopyButton getText={() => text} />
    {#if markdown}
      <RawToggleButton pressed={raw} onclick={toggleRaw} />
    {/if}
  </div>
{/if}
