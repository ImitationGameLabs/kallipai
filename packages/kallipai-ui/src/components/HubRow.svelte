<script lang="ts">
  // One hub row (hub-style pages): full-width with a
  // 64px touch target -- the bar's icon-only cells are smaller because their hit
  // area is the whole grid cell; here the row IS the target. No chevron:
  // the row itself reads as the destination. `href` renders an anchor,
  // `onclick` a button (type="button": an action has no destination and
  // must not push a history entry, and it stretches with `w-full
  // text-left` because a bare button sizes to its content). The two props
  // are mutually exclusive and `href` wins: a row passed both silently
  // drops `onclick`. Purely presentational: `label` arrives already evaluated,
  // Leading mark: `Icon` (size-7) or a four-state `indicator` dot.
  import type { Component } from "svelte";
  import {
    navIndicatorDotClass,
    navIndicatorLabel,
    type NavIndicator,
  } from "../lib/shell.ts";
  import { badgeLabel } from "../lib/session/unread.svelte.ts";
  import { chat_unread_badge_aria } from "../paraglide/messages.js";

  let {
    href = undefined,
    onclick = undefined,
    Icon = undefined,
    label,
    indicator,
    badge = 0,
  }: {
    href?: string;
    onclick?: () => void;
    Icon?: Component;
    label: string;
    indicator?: NavIndicator;
    /** The unread count (rendered through badgeLabel; 0 renders nothing). */
    badge?: number;
  } = $props();
</script>

{#snippet rowContent()}
  {#if indicator === "pending"}
    <!-- Spinning ring for pending (mirrors the sidebar dot: a filled dot has
           no visible rotation axis); size scaled to the row's size-7 icon. -->
    <span
      class="size-3 rounded-full border-2 border-surface-400-600 border-t-transparent animate-spin shrink-0 opacity-70"
      aria-hidden="true"
    ></span>
  {:else if indicator}
    <span
      class="size-3 rounded-full shrink-0 opacity-70 {navIndicatorDotClass(
        indicator,
      )}"
      aria-hidden="true"
    ></span>
  {:else if Icon}
    <Icon class="size-7 shrink-0 opacity-70" aria-hidden="true" />
  {/if}
  <span class="text-lg font-medium">{label}</span>
  {#if indicator}
    <span class="sr-only">{navIndicatorLabel(indicator)}</span>
  {/if}
  {#if badge > 0}
    <span
      class="ml-auto shrink-0 rounded-full px-1.5 py-0.5 text-[11px] leading-none font-semibold preset-filled-primary-500"
    >
      {badgeLabel(badge)}
    </span>
    <span class="sr-only"
      >{chat_unread_badge_aria({ count: badgeLabel(badge) })}</span
    >
  {/if}
{/snippet}

{#if href}
  <a
    {href}
    class="flex items-center gap-4 min-h-16 px-4 hover:preset-filled-surface-500 transition-colors"
  >
    {@render rowContent()}
  </a>
{:else}
  <button
    type="button"
    {onclick}
    class="flex items-center gap-4 min-h-16 px-4 w-full text-left hover:preset-filled-surface-500 transition-colors"
  >
    {@render rowContent()}
  </button>
{/if}
