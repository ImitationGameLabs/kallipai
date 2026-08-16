<script lang="ts">
  import { barFillPct, barColorClass } from "../../lib/manage/compute.ts";
  import { manage_budget_heading } from "../../paraglide/messages.js";

  // Reusable budget progress bar with color-coded fill.
  // Green: remaining > 60% of budget.
  // Amber: remaining 15–60% of budget.
  // Red:   remaining < 15% of budget.
  // Neutral (no fill): budget is 0 (unset or cleared).

  let {
    consumed,
    budget,
    label,
  }: {
    consumed: number;
    budget: number;
    label?: string;
  } = $props();

  const pct = $derived(barFillPct(consumed, budget));
  const barClass = $derived(barColorClass(consumed, budget));
</script>

<div class="w-full">
  {#if label}
    <div class="text-xs opacity-60 mb-1">{label}</div>
  {/if}
  <div
    class="w-full h-3 rounded-full bg-surface-200-800 overflow-hidden"
    role="progressbar"
    aria-label={label ?? manage_budget_heading()}
    aria-valuenow={pct}
    aria-valuemin={0}
    aria-valuemax={100}
  >
    <div
      class="h-full rounded-full transition-all duration-300 {barClass}"
      style="width: {pct}%"
    ></div>
  </div>
</div>
