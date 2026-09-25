<script lang="ts">
  // The agents drawer body: root-first row list with every row always
  // visible (vertical scroll, no fold), and a live active/total summary
  // plus the compact budget bar up top. Purely presentational: the
  // store lives with the host.
  import { agentStateLabel } from "../lib/agentState.ts";
  import AgentStateIcon from "./AgentStateIcon.svelte";
  import { parkedReasonText } from "../lib/session/parkedReason.ts";
  import {
    contextOccupancy,
    formatTokenCount,
    type TagmaStatusSummary,
  } from "../lib/tagmata.svelte.ts";
  import type { StatusCardRow } from "../lib/session/statusCard.svelte.ts";
  import {
    tagma_drawer_title,
    tagma_drawer_aria,
    tagma_status_budget,
    tagma_status_root,
    tagma_status_waiting,
    tagma_status_unlimited,
  } from "../paraglide/messages.js";

  let {
    rootRow,
    subRows,
    budget,
  }: {
    rootRow: StatusCardRow | null;
    subRows: readonly StatusCardRow[];
    /** Live aggregate for the summary line; absent until the first tick. */
    budget?: TagmaStatusSummary | undefined;
  } = $props();

  // Root anchors the list; subs follow in the store's attention order.
  const rows = $derived(rootRow ? [rootRow, ...subRows] : [...subRows]);
  // Same aggregation the status line uses: root busy counts toward the
  // active numerator, and the root always holds one total slot.
  const active = $derived(
    budget ? (budget.rootState === "busy" ? 1 : 0) + budget.subagentsActive : 0,
  );
  const total = $derived(budget ? 1 + budget.subagentsTotal : 0);
  const budgetPct = $derived(
    budget && budget.tokenBudget > 0
      ? Math.min(100, (budget.tokenConsumed / budget.tokenBudget) * 100)
      : 0,
  );
</script>

<div class="flex flex-col h-full min-h-0" aria-label={tagma_drawer_aria()}>
  <div class="px-4 py-3 border-b border-surface-200-800 flex flex-col gap-2">
    <h2 class="text-sm font-semibold uppercase tracking-wider opacity-60">
      {tagma_drawer_title()}
    </h2>
    <div class="flex items-center gap-2 min-w-0">
      {#if budget}
        <span
          class="size-2 rounded-full shrink-0 {active > 0
            ? 'bg-success-500'
            : 'bg-surface-400-600'}"
          aria-hidden="true"
        ></span>
        <span class="tabular-nums whitespace-nowrap text-sm"
          >{active}/{total}</span
        >
        <span class="flex-1"></span>
        <span class="text-sm opacity-60">{tagma_status_budget()}</span>
        {#if !budget.tokenBudgetUnlimited}
          <div
            class="h-1.5 w-20 rounded-full bg-surface-400-600 overflow-hidden"
          >
            <div
              class="h-full rounded-full bg-primary-500 transition-[width] duration-500"
              style="width: {budgetPct}%"
            ></div>
          </div>
        {/if}
        <span class="tabular-nums whitespace-nowrap text-sm">
          {formatTokenCount(budget.tokenConsumed)}
          /
          {budget.tokenBudgetUnlimited
            ? tagma_status_unlimited()
            : formatTokenCount(budget.tokenBudget)}
        </span>
      {:else}
        <span
          class="size-2 rounded-full bg-surface-400-600 animate-pulse"
          aria-hidden="true"
        ></span>
        <span class="text-sm opacity-50">{tagma_status_waiting()}</span>
      {/if}
    </div>
  </div>
  <div class="flex-1 min-h-0 overflow-y-auto touch-pan-y px-2 py-1">
    {#each rows as row, i (row.id)}
      <div class="py-1.5 px-2 rounded-base">
        <div class="flex items-center gap-2 min-w-0">
          <AgentStateIcon state={row.state} />
          <span class="font-medium truncate">{row.role || row.id}</span>
          {#if i === 0 && rootRow}
            <span
              class="text-xs px-1.5 py-0.5 rounded-full preset-tonal-surface shrink-0"
              >{tagma_status_root()}</span
            >
          {/if}
          <span class="flex-1"></span>
          <span class="tabular-nums whitespace-nowrap text-sm opacity-80">
            {contextOccupancy(row.contextTokens, row.contextWindow)}
          </span>
        </div>
        {#if row.activity}
          <div class="truncate text-sm opacity-60 ps-7">{row.activity}</div>
        {/if}
        {#if row.parkedReason}
          <div class="truncate text-sm text-warning-600-400 ps-7">
            {parkedReasonText(row.parkedReason)}
          </div>
        {/if}
      </div>
    {/each}
  </div>
</div>
