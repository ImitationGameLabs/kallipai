<script lang="ts">
  // The composer's pending-quote strip: who is being quoted, one line of the
  // quoted body, and the way out. It sits above the input card and holds the
  // whole quotation state for the user -- one quote at a time, so quoting again
  // simply replaces the strip's contents.
  //
  // Cancel is reachable three ways: this button, Escape inside the composer
  // (Composer forwards it to `onCancelQuote`), and sending, which clears the
  // strip. Escape keeps the caret in the textarea, so the key never eats
  // typed text.
  import { X } from "@lucide/svelte";
  import {
    chat_quote_bar_aria,
    chat_quote_cancel_aria,
    chat_quote_bar_preview_aria,
    chat_quote_unknown_author,
  } from "../paraglide/messages.js";

  let {
    author,
    preview,
    onCancel,
  }: {
    /** Quoted author's handle; absent for rows cached before senders were
     *  tracked, which fall back to a neutral label. */
    author?: string;
    preview?: string;
    onCancel: () => void;
  } = $props();

  const label = $derived(author ?? chat_quote_unknown_author());
</script>

<div
  class="mb-1 flex min-w-0 items-center gap-2 rounded-base border-l-2 border-surface-400-600 bg-surface-100-900/80 px-2 py-1 text-xs"
  role="group"
  aria-label={chat_quote_bar_aria({ author: label })}
>
  <div class="min-w-0 flex-1">
    <span class="block truncate font-medium opacity-70">{label}</span>
    {#if preview !== undefined}
      <span class="block truncate opacity-90">
        <span class="sr-only">{chat_quote_bar_preview_aria()}</span>{preview}
      </span>
    {/if}
  </div>
  <button
    type="button"
    class="shrink-0 rounded p-1 opacity-60 hover:bg-surface-200-800 hover:opacity-100 cursor-pointer"
    aria-label={chat_quote_cancel_aria()}
    onclick={onCancel}
  >
    <X class="size-3.5" aria-hidden="true" />
  </button>
</div>
