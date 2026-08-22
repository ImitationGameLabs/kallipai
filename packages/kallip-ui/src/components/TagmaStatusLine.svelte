<script lang="ts">
  // The one-line status summary for the mobile chat top row: liveness dot +
  // active/total counts (or the waiting placeholder). The state toggle rides
  // the line's right edge with the same anchor in both states, so expanding
  // never shifts it; the expanded half (budget + agent rows) renders in
  // TagmaStatusPanel below the row, keeping this line the row's only height.
  import { ChevronDown, ChevronUp } from "@lucide/svelte";
  import {
    tagma_status_show_details,
    tagma_status_waiting,
  } from "../paraglide/messages.js";
  import type { TagmaStatusSummary } from "../lib/tagmata.svelte.ts";

  let {
    status,
    expanded = false,
    onToggle,
  }: {
    status: TagmaStatusSummary | undefined;
    expanded?: boolean;
    onToggle?: () => void;
  } = $props();
</script>

<div
  class="relative mx-auto w-full max-w-[56rem] px-4 min-h-10 flex items-center gap-3 text-base"
>
  {#if status}
    <span
      class="size-2 rounded-full shrink-0 {status.subagentsActive > 0
        ? 'bg-success-500'
        : 'bg-surface-400-600'}"
      aria-hidden="true"
    ></span>
    <span class="tabular-nums whitespace-nowrap"
      >{status.subagentsActive}/{status.subagentsTotal}</span
    >
  {:else}
    <span
      class="size-2 rounded-full bg-surface-400-600 animate-pulse"
      aria-hidden="true"
    ></span>
    <span class="opacity-50">{tagma_status_waiting()}</span>
  {/if}
  <button
    type="button"
    onclick={onToggle}
    class="size-10 grid place-items-center rounded-base opacity-50 hover:opacity-100 hover:preset-filled-surface-500 shrink-0 absolute right-2 top-1/2 -translate-y-1/2"
    aria-label={tagma_status_show_details()}
    aria-expanded={expanded}
  >
    {#if expanded}
      <ChevronUp class="size-4" aria-hidden="true" />
    {:else}
      <ChevronDown class="size-4" aria-hidden="true" />
    {/if}
  </button>
</div>
