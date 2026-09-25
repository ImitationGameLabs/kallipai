<script lang="ts">
  import { barFillPct, barColorClass } from "../../lib/manage/compute.ts";
  import {
    manage_budget_heading,
    manage_budget_unlimited,
  } from "../../paraglide/messages.js";

  // Reusable budget progress bar with color-coded fill.
  // Green: remaining > 60% of budget.
  // Amber: remaining 15–60% of budget.
  // Red:   remaining < 15% of budget.
  // Neutral (no fill): budget is 0 (unset or cleared). An unlimited budget
  // renders the same neutral track with an Unlimited badge overlay.

  let {
    consumed,
    budget,
    label,
    unlimited = false,
  }: {
    consumed: number;
    budget: number;
    label?: string;
    unlimited?: boolean;
  } = $props();

  const pct = $derived(barFillPct(consumed, budget));
  const barClass = $derived(barColorClass(consumed, budget));
</script>

<div class="w-full">
  {#if label}
    <div class="text-xs opacity-60 mb-1">{label}</div>
  {/if}
  <div class="relative">
    <div
      class="w-full h-3 rounded-full bg-surface-200-800 overflow-hidden"
      role="progressbar"
      aria-label={label ?? manage_budget_heading()}
      aria-valuenow={pct}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuetext={unlimited ? manage_budget_unlimited() : undefined}
    >
      {#if !unlimited}
        <div
          class="h-full rounded-full transition-all duration-300 {barClass}"
          style="width: {pct}%"
        ></div>
      {/if}
    </div>
    {#if unlimited}
      <span
        class="absolute inset-y-0 right-2 flex items-center text-[10px] font-medium uppercase tracking-wider opacity-70"
      >
        {manage_budget_unlimited()}
      </span>
    {/if}
  </div>
</div>
