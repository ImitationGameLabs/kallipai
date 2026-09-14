<script lang="ts">
  // The top-bar pill row: one short chip per agent (root first), an
  // overflow chip folding the rest, and the compact budget indicator
  // pinned right. Chips use the Skeleton chip utility with the
  // filter-chip pattern shared with the schedule editor (outlined with
  // a filled hover at rest, filled-primary while the pill's detail
  // popover is open). State marks come from the shared icon rendering
  // (AgentStateIcon); the state word rides the chip's title/aria and
  // the detail layer. Purely presentational: rows
  // come from the status card store via the page; the overflow hands
  // back to the parent's roster dialog.
  import { agentStateLabel } from "../lib/agentState.ts";
  import AgentStateIcon from "./AgentStateIcon.svelte";
  import {
    formatTokenCount,
    type TagmaStatusSummary,
  } from "../lib/tagmata.svelte.ts";
  import { visiblePills } from "../lib/agentPillLayout.ts";
  import type { StatusCardRow } from "../lib/session/statusCard.svelte.ts";
  import {
    tagma_status_aria,
    tagma_status_budget,
    tagma_status_more,
  } from "../paraglide/messages.js";
  import { Popover } from "@skeletonlabs/skeleton-svelte";
  import AgentDetailPanel from "./AgentDetailPanel.svelte";

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
  const pills = $derived(visiblePills(rows, 5));
  // Which agent's detail popover is open (one at a time).
  let openId = $state<string | null>(null);
</script>

<div class="flex items-center gap-1.5 min-w-0" aria-label={tagma_status_aria()}>
  {#each pills.visible as row (row.id)}
    <Popover
      open={openId === row.id}
      onOpenChange={(e) => (openId = e.open ? row.id : null)}
    >
      <Popover.Anchor>
        <button
          type="button"
          class="chip {openId === row.id
            ? 'preset-filled-primary-200-800 border border-transparent'
            : 'preset-outlined-surface-500 hover:preset-filled-surface-500'} max-w-[10rem]"
          title={agentStateLabel(row.state)}
          aria-label="{row.role || row.id} — {agentStateLabel(row.state)}"
          aria-expanded={openId === row.id}
          onclick={() => (openId = openId === row.id ? null : row.id)}
        >
          <span class="badge" aria-hidden="true">
            <AgentStateIcon state={row.state} size="size-3" label={false} />
          </span>
          <span class="truncate">{row.role || row.id}</span>
        </button>
      </Popover.Anchor>
      {#if openId === row.id}
        <Popover.Positioner>
          <Popover.Content class="max-w-[20rem] p-0">
            <AgentDetailPanel {row} />
          </Popover.Content>
        </Popover.Positioner>
      {/if}
    </Popover>
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
  {#if budget}
    <div
      class="ms-auto flex items-center gap-1.5 shrink-0"
      title={tagma_status_budget()}
      aria-label={tagma_status_budget()}
    >
      <progress
        class="progress h-1.5 w-16"
        value={budget.tokenConsumed}
        max={budget.tokenBudget || 1}
      ></progress>
      <span class="tabular-nums whitespace-nowrap text-sm">
        {formatTokenCount(budget.tokenConsumed)}
        /
        {formatTokenCount(budget.tokenBudget)}
      </span>
    </div>
  {/if}
</div>
