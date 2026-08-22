<script lang="ts">
  // The expanded half of the mobile status area: the budget row plus the
  // agent rows, rendered full-width BELOW the top row (AppShell topPanel)
  // rather than inside it -- this keeps the top row one line tall and the
  // budget bar unclamped by the row's centre cell. Mirrors the expanded
  // TagmaStatusHeader markup (its collapsed/expended split stays intact for
  // the online chat and desktop paths).
  import { ChevronDown } from "@lucide/svelte";
  import {
    formatTokenCount,
    type TagmaStatusSummary,
  } from "../lib/tagmata.svelte.ts";
  import {
    tagma_status_budget,
    tagma_status_waiting,
  } from "../paraglide/messages.js";
  import TagmaAgentRows from "./TagmaAgentRows.svelte";
  import type { StatusCardRow } from "../lib/session/statusCard.svelte.ts";

  let {
    status,
    agentRows,
  }: {
    status: TagmaStatusSummary | undefined;
    agentRows?: {
      rootRow: StatusCardRow | null;
      subRows: readonly StatusCardRow[];
    };
  } = $props();

  // Budget fill width, clamped to [0, 100]. 0 budget -> 0% (avoids div-by-zero).
  const budgetPct = $derived(
    status && status.tokenBudget > 0
      ? Math.min(100, (status.tokenConsumed / status.tokenBudget) * 100)
      : 0,
  );
</script>

<div class="border-b border-surface-200-800">
  <div
    class="mx-auto w-full max-w-[56rem] px-4 py-3 flex flex-col items-center gap-y-2 text-lg"
  >
    {#if status}
      <div class="flex items-center gap-2">
        <span class="text-base opacity-60">{tagma_status_budget()}</span>
        <div
          class="h-2 w-56 shrink-0 rounded-full bg-surface-400-600 overflow-hidden"
        >
          <div
            class="h-full rounded-full bg-primary-500 transition-[width] duration-500"
            style="width: {budgetPct}%"
          ></div>
        </div>
        <span class="tabular-nums whitespace-nowrap text-base">
          {formatTokenCount(status.tokenConsumed)} / {formatTokenCount(
            status.tokenBudget,
          )}
        </span>
      </div>
    {:else}
      <div class="flex items-center gap-1.5 text-base opacity-50">
        <span
          class="size-2 rounded-full bg-surface-400-600 animate-pulse"
          aria-hidden="true"
        ></span>
        <span>{tagma_status_waiting()}</span>
      </div>
    {/if}
  </div>
  {#if agentRows}
    <TagmaAgentRows
      rootRow={agentRows.rootRow}
      subRows={agentRows.subRows}
      narrow={false}
    />
  {/if}
</div>
