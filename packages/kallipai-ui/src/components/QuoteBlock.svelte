<script lang="ts">
  // The quote strip that sits on top of a message bubble. It renders the view
  // the quotes controller resolved for a quoted id (see lib/quote.ts): the
  // author and a one-line preview once the row is loaded or a probe fetched
  // it, or -- when the target is gone, off the loaded window, or the probe
  // failed -- the reason. Nothing here fetches: a click asks the parent to
  // scroll to the original, and only a failed load offers a retry.
  //
  // The states stay visually distinct: found is a clickable block; a deleted
  // target, a row outside the loaded window, and a failed probe are inert copy;
  // and only the failed probe carries a retry. Collapsing them would invite a
  // retry for a message that cannot be fetched or no longer exists.
  import { Quote } from "@lucide/svelte";
  import type { QuoteView } from "../lib/quote.ts";
  import {
    chat_quote_block_aria,
    chat_quote_deleted,
    chat_quote_off_window,
    chat_quote_retry,
    chat_quote_unavailable,
    chat_quote_unknown_author,
  } from "../paraglide/messages.js";

  let {
    view,
    onClick,
    onRetry,
    describedBy,
  }: {
    view: QuoteView;
    /** Scroll to the quoted line. Absent, the block is inert copy. */
    onClick?: () => void;
    /** Re-attempt the load of an `unavailable` target. */
    onRetry?: () => void;
    /** Id of the accessible label of the quoted row, when that row is
     *  rendered: a screen reader then hears which message this quotes
     *  before the block's own copy, which stays readable. */
    describedBy?: string;
  } = $props();

  const MARK =
    "mb-1.5 w-full min-w-0 rounded-base border-l-2 border-surface-400-600 bg-surface-100-900/70 px-2 py-1 text-left text-xs";
</script>

{#if view.status === "found"}
  {#if onClick}
    <button
      type="button"
      class="{MARK} cursor-pointer hover:bg-surface-200-800"
      onclick={onClick}
      aria-describedby={describedBy}
    >
      <span class="sr-only">{chat_quote_block_aria()}</span>
      <span class="block truncate font-medium opacity-70">
        {view.author ?? chat_quote_unknown_author()}
      </span>
      <span class="block truncate opacity-90">
        {view.preview}
      </span></button
    >
  {:else}
    <div class={MARK} aria-describedby={describedBy}>
      <span class="sr-only">{chat_quote_block_aria()}</span>
      <span class="block truncate font-medium opacity-70">
        {view.author ?? chat_quote_unknown_author()}
      </span>
      <span class="block truncate opacity-90">
        {view.preview}
      </span>
    </div>
  {/if}
{:else}
  <div class="{MARK} flex items-center gap-1.5 italic opacity-70">
    <Quote class="size-3 shrink-0" aria-hidden="true" />
    <span class="min-w-0 flex-1 truncate">
      {view.status === "deleted"
        ? chat_quote_deleted()
        : view.status === "off-window"
          ? chat_quote_off_window()
          : chat_quote_unavailable()}
    </span>
    {#if view.status === "unavailable" && onRetry}
      <button
        type="button"
        class="shrink-0 not-italic underline underline-offset-2 hover:opacity-100 cursor-pointer"
        onclick={onRetry}
      >
        {chat_quote_retry()}
      </button>
    {/if}
  </div>
{/if}
