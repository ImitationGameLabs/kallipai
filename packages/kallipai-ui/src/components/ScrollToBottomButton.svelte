<script lang="ts">
  // Floating "jump to latest" pill for a detached transcript viewport. The
  // parent owns visibility (renders this only while !follow) and positioning
  // (an overlay wrapper); this owns only the shape: an outlined pill that
  // fills on hover, the missed count as a HubRow-style badge, and a polite
  // live region so count changes are announced while the pill is visible.
  import { ArrowDown } from "@lucide/svelte";
  import {
    chat_jump_to_latest,
    chat_new_while_away_aria,
  } from "../paraglide/messages.js";
  import { badgeLabel } from "../lib/session/unread.svelte.ts";

  let {
    missed = 0,
    onclick,
  }: {
    /** Lines that landed while the user was scrolled away. */
    missed?: number;
    onclick: () => void;
  } = $props();
</script>

<button
  type="button"
  {onclick}
  title={chat_jump_to_latest()}
  aria-label={chat_jump_to_latest()}
  aria-live="polite"
  class="pointer-events-auto relative flex size-10 items-center justify-center rounded-full preset-outlined-surface-500 shadow-lg transition-colors hover:preset-filled-primary-500"
>
  <ArrowDown class="size-5" aria-hidden="true" />
  {#if missed > 0}
    <span
      class="absolute -right-1 -top-1 rounded-full px-1.5 py-0.5 text-[11px] leading-none font-semibold preset-filled-primary-500"
      aria-hidden="true"
    >
      {badgeLabel(missed)}
    </span>
    <span class="sr-only">
      {chat_new_while_away_aria({ count: badgeLabel(missed) })}
    </span>
  {/if}
</button>
