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
  import Markdown from "./Markdown.svelte";
  import CopyButton from "./CopyButton.svelte";
  import RawToggleButton from "./RawToggleButton.svelte";
  import type { TogglePin } from "../lib/transcript.svelte.ts";

  let {
    text,
    markdown = false,
    mine = false,
    bare = false,
    pending = false,
    pin,
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
    /** Parent's scroll-pin controller, invoked on a raw toggle. Optional so the bubble can
     *  render outside a scroll context (no pinning). */
    pin?: TogglePin;
  } = $props();

  // Per-bubble raw-source view: ephemeral, resets when the bubble unmounts.
  let raw = $state(false);
  let box: HTMLDivElement | undefined = $state();
  let actions: HTMLDivElement | undefined = $state();

  function toggleRaw(): void {
    raw = !raw;
    if (box && actions) pin?.(box, actions);
  }
</script>

<div
  bind:this={box}
  class="max-w-[80%] min-w-0 rounded-base px-3 py-2 text-sm {mine
    ? 'preset-filled-primary-100-900'
    : 'preset-tonal-surface'} {!markdown
    ? 'whitespace-pre-wrap break-words'
    : ''} {pending ? 'opacity-60' : ''}"
>
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
</div>
{#if !bare}
  <div bind:this={actions} class="flex items-center gap-1">
    <CopyButton getText={() => text} />
    {#if markdown}
      <RawToggleButton pressed={raw} onclick={toggleRaw} />
    {/if}
  </div>
{/if}
