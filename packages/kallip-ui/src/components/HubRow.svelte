<script lang="ts">
  // One hub row (ManageHubPage / AccountHubPage): full-width with a 48px
  // touch target -- the bar's icon-only cells are smaller because their hit
  // area is the whole grid cell; here the row IS the target. No chevron:
  // the row itself reads as the destination. `href` renders an anchor,
  // `onclick` a button (type="button": an action has no destination and
  // must not push a history entry, and it stretches with `w-full
  // text-left` because a bare button sizes to its content). The two props
  // are mutually exclusive and `href` wins: a row passed both silently
  // drops `onclick`. Purely presentational: `label` arrives already
  // evaluated, so i18n calls stay in the page.
  import type { Component } from "svelte";

  let {
    href = undefined,
    onclick = undefined,
    Icon,
    label,
  }: {
    href?: string;
    onclick?: () => void;
    Icon: Component;
    label: string;
  } = $props();
</script>

{#if href}
  <a
    {href}
    class="flex items-center gap-3 min-h-12 px-4 hover:preset-filled-surface-500 transition-colors"
  >
    <Icon class="size-5 shrink-0 opacity-70" aria-hidden="true" />
    <span class="text-sm font-medium">{label}</span>
  </a>
{:else}
  <button
    type="button"
    {onclick}
    class="flex items-center gap-3 min-h-12 px-4 w-full text-left hover:preset-filled-surface-500 transition-colors"
  >
    <Icon class="size-5 shrink-0 opacity-70" aria-hidden="true" />
    <span class="text-sm font-medium">{label}</span>
  </button>
{/if}
