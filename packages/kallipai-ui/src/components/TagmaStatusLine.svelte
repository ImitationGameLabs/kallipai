<script lang="ts">
  // The one-line status summary for the mobile chat top row -- and the
  // whole row is the agents-drawer entry: liveness dot + active/total
  // counts (or the waiting placeholder), a light trailing chevron, and
  // press feedback; no border, no button chrome. The expanded half
  // lives in the drawer, so the line stays the row's only height.
  // Purely presentational: the snapshot comes from the channel store.
  import { ChevronDown } from "@lucide/svelte";
  import {
    tagma_drawer_open,
    tagma_status_waiting,
  } from "../paraglide/messages.js";
  import type { TagmaStatusSummary } from "../lib/tagmata.svelte.ts";

  let {
    status,
    onOpen,
    expanded,
  }: {
    status: TagmaStatusSummary | undefined;
    onOpen?: () => void;
    /** Whether the agents drawer this control opens is currently open */
    expanded?: boolean;
  } = $props();
  // The whole-row label carries the row's visible information in one
  // read: with a live summary the counts ride the drawer-open label,
  // otherwise the waiting placeholder stands alone.
  const rowLabel = $derived(
    status
      ? `${tagma_drawer_open()} ${status.subagentsActive}/${status.subagentsTotal}`
      : tagma_drawer_open(),
  );
</script>

<button
  type="button"
  onclick={onOpen}
  aria-haspopup="dialog"
  aria-label={rowLabel}
  aria-expanded={expanded ? "true" : "false"}
  class="relative mx-auto w-full max-w-[56rem] px-4 min-h-10 flex items-center gap-3 text-base rounded-base active:preset-tonal-surface"
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
  <ChevronDown class="size-4 opacity-50 shrink-0 ml-auto" aria-hidden="true" />
</button>
