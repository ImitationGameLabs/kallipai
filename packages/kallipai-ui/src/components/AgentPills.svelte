<script lang="ts">
  // The top-bar pill row: one short label per agent (root first),
  // wrapping to at most two rows; whatever does not fit folds into an
  // overflow chip, and the compact budget indicator pins right, out
  // of the wrap flow. Pills are non-interactive labels: the tooltip
  // carries the agent's full name plus its state word (the visible
  // text is the name itself, so the readout needs no aria-label).
  // State marks come from the shared icon rendering (AgentStateIcon).
  // Purely presentational: rows come from the status card store via
  // the page; the overflow chip hands back to the parent's transient
  // side panel.
  import { agentStateLabel } from "../lib/agentState.ts";
  import AgentStateIcon from "./AgentStateIcon.svelte";
  import {
    formatTokenCount,
    type TagmaStatusSummary,
  } from "../lib/tagmata.svelte.ts";
  import {
    visiblePills,
    visibleCountWithinRows,
  } from "../lib/agentPillLayout.ts";
  import type { StatusCardRow } from "../lib/session/statusCard.svelte.ts";
  import {
    tagma_status_aria,
    tagma_status_budget,
    tagma_status_more,
    tagma_status_unlimited,
  } from "../paraglide/messages.js";

  let {
    rootRow,
    subRows,
    budget,
    onOverflow,
  }: {
    rootRow: StatusCardRow | null;
    subRows: readonly StatusCardRow[];
    /** Live aggregate for the right-aligned compact budget indicator. */
    budget?: TagmaStatusSummary | undefined;
    onOverflow?: () => void;
  } = $props();

  // Root anchors the row; subs follow in the store's attention order.
  const rows = $derived(rootRow ? [rootRow, ...subRows] : [...subRows]);
  // Two-row budget: measured, not assumed. A hidden mirror row always
  // renders the full roster in the same slot (invisible, inert), so its
  // wrap geometry is the natural full-display layout at any moment. The
  // cap is read off the mirror and applied atomically to the visible
  // row, so the visible pills never show an uncapped layout.
  let cap = $state(Number.POSITIVE_INFINITY);
  const pills = $derived(visiblePills(rows, cap));
  let mirror: HTMLDivElement | null = $state(null);

  const measure = () => {
    const box = mirror;
    if (!box) return;
    const tops = Array.from(box.children).map(
      (el) => (el as HTMLElement).offsetTop,
    );
    const fit = visibleCountWithinRows(tops, 2);
    const overflow = box.children.length - fit;
    cap = overflow > 0 ? fit - 1 : fit;
  };

  // Synchronous read on every entry (mount, roster change, resize):
  // effects run after the DOM update and the mirror holds the full
  // display, so one read settles the cap -- no waiting on later frames
  // and no dependence on further events to converge.
  $effect(() => {
    void rows;
    measure();
  });

  // Viewport resizes re-measure off the mirror: it fills the same slot
  // as the visible row, and observing it keeps cap writes out of the
  // observer's own feedback loop.
  $effect(() => {
    const box = mirror;
    if (!box) return;
    const observer = new ResizeObserver(() => measure());
    observer.observe(box);
    return () => observer.disconnect();
  });
</script>

<div class="flex items-center gap-3 min-w-0" aria-label={tagma_status_aria()}>
  <div class="relative flex-1 min-w-0">
    <div class="flex flex-wrap items-center gap-1.5">
      {#each pills.visible as row (row.id)}
        <span
          class="chip preset-outlined-surface-500 max-w-[10rem]"
          title="{row.role || row.id} - {agentStateLabel(row.state)}"
        >
          <span class="badge" aria-hidden="true">
            <AgentStateIcon state={row.state} size="size-3" label={false} />
          </span>
          <span class="truncate">{row.role || row.id}</span>
        </span>
      {/each}
      {#if pills.overflow > 0}
        <button
          type="button"
          class="chip preset-outlined-surface-500 shrink-0"
          title={tagma_status_more({ count: pills.overflow })}
          aria-label={tagma_status_more({ count: pills.overflow })}
          aria-haspopup="dialog"
          onclick={() => onOverflow?.()}
        >
          +{pills.overflow}
        </button>
      {/if}
    </div>
    <div
      class="absolute inset-0 flex flex-wrap items-center gap-1.5 invisible pointer-events-none"
      aria-hidden="true"
      bind:this={mirror}
    >
      {#each rows as row (row.id)}
        <span class="chip preset-outlined-surface-500 max-w-[10rem]">
          <span class="badge" aria-hidden="true">
            <AgentStateIcon state={row.state} size="size-3" label={false} />
          </span>
          <span class="truncate">{row.role || row.id}</span>
        </span>
      {/each}
    </div>
  </div>
  {#if budget}
    <div
      class="flex items-center gap-1.5 shrink-0"
      title={tagma_status_budget()}
      aria-label={tagma_status_budget()}
    >
      {#if !budget.tokenBudgetUnlimited}
        <progress
          class="progress h-1.5 w-16"
          value={budget.tokenConsumed}
          max={budget.tokenBudget || 1}
        ></progress>
      {/if}
      <span class="tabular-nums whitespace-nowrap text-sm">
        {formatTokenCount(budget.tokenConsumed)}
        /
        {budget.tokenBudgetUnlimited
          ? tagma_status_unlimited()
          : formatTokenCount(budget.tokenBudget)}
      </span>
    </div>
  {/if}
</div>
