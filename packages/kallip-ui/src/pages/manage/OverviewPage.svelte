<script lang="ts">
  import { budgetStore } from "../../lib/manage/budget.svelte.ts";
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import { usageStore } from "../../lib/manage/usage.svelte.ts";
  import { formatTokenCount } from "../../lib/tagmata.svelte.ts";
  import BudgetBar from "../../components/manage/BudgetBar.svelte";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import {
    manage_overview_title,
    manage_overview_heading,
    manage_budget_heading,
    manage_budget_tokens,
    manage_overview_budget_consumed_line,
    manage_agents_heading,
    manage_overview_agents_idle,
    manage_overview_agents_busy,
    manage_overview_agents_faulted,
    manage_overview_quick_actions,
    manage_overview_100m_budget,
    manage_budget_clear,
    manage_budget_clear_desc,
    common_clear,
    manage_overview_configuration,
    manage_overview_profiles_link,
    manage_overview_schedules_link,
    manage_overview_usage_heading,
    manage_overview_usage_prompt_tokens,
    manage_overview_usage_cache_read,
    manage_overview_usage_hit_rate,
    manage_overview_usage_scope_hint,
  } from "../../paraglide/messages.js";

  $effect(() => {
    budgetStore.startPolling(30_000);
    agentsStore.startPolling(30_000);
    // The totals ride on any agent status response; the roster's first
    // live agent is only the request carrier. Faulted agents reject the
    // status endpoint (409 "agent is faulted; no status"), so they
    // cannot carry; with no live agent the totals stay null and the
    // usage rows hide.
    usageStore.startPolling(
      () => agentsStore.agents.find((a) => a.state !== "faulted")?.id,
      30_000,
    );
    return () => {
      budgetStore.stopPolling();
      agentsStore.stopPolling();
      usageStore.stopPolling();
    };
  });

  // Kick the totals fetch once the roster first arrives: startPolling's
  // immediate refresh runs before the roster exists and no-ops then.
  $effect(() => {
    if (agentsStore.agents.length > 0) usageStore.refresh();
  });

  let { basePath = "/local/manage" }: { basePath?: string } = $props();
  let showPauseDialog = $state(false);

  async function onConfirmPause() {
    try {
      await budgetStore.pauseAll();
      showPauseDialog = false;
    } catch {}
  }
</script>

<svelte:head><title>{manage_overview_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="px-2 md:p-6 max-w-2xl space-y-6">
    <!-- md+ keeps this h1; below md the shell top row carries the title (AppShell `title`). -->
    <h1 class="text-xl font-semibold hidden md:block">
      {manage_overview_heading()}
    </h1>

    {#if budgetStore.error}
      <p class="text-error-500 dark:text-error-400 text-sm">
        {budgetStore.error}
      </p>
    {/if}

    <div class="grid grid-cols-1 sm:grid-cols-2 gap-4">
      <!-- Budget -->
      <a
        href={`${basePath}/budget`}
        class="card preset-tonal-surface p-5 space-y-2 hover:preset-filled-surface-400 transition"
      >
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_budget_heading()}
        </h2>
        <BudgetBar
          consumed={budgetStore.consumed}
          budget={budgetStore.budget}
          unlimited={budgetStore.unlimited}
        />
        <div class="text-sm">
          {#if budgetStore.unlimited}
            {manage_budget_tokens({
              count: formatTokenCount(budgetStore.consumed),
            })}
          {:else}
            {manage_overview_budget_consumed_line({
              pct: budgetStore.consumedPct,
              remaining: formatTokenCount(budgetStore.remaining),
            })}
          {/if}
        </div>
      </a>

      <!-- Agents -->
      <a
        href={`${basePath}/agents`}
        class="card preset-tonal-surface p-5 space-y-2 hover:preset-filled-surface-400 transition"
      >
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_agents_heading()}
        </h2>
        <div class="text-sm space-y-1">
          <div>
            {manage_overview_agents_idle({ count: agentsStore.idleCount })}
          </div>
          <div>
            {manage_overview_agents_busy({ count: agentsStore.busyCount })}
          </div>
          {#if agentsStore.faultedCount > 0}
            <div class="text-error-500 dark:text-error-400">
              {manage_overview_agents_faulted({
                count: agentsStore.faultedCount,
              })}
            </div>
          {/if}
        </div>
      </a>

      <!-- Token usage: tagma-wide, this launch. The numbers ride on one
           status request (the roster's first non-faulted agent as
           carrier); the usage rows stay hidden until the first totals
           arrive. -->
      <div class="card preset-tonal-surface p-5 space-y-2">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_overview_usage_heading()}
        </h2>
        {#if usageStore.totals}
          <div class="text-sm space-y-1">
            <div>
              {manage_overview_usage_prompt_tokens({
                count: formatTokenCount(usageStore.totals.prompt_tokens),
              })}
            </div>
            <div>
              {manage_overview_usage_cache_read({
                count: formatTokenCount(usageStore.totals.cache_read_tokens),
              })}
            </div>
            <div>
              {manage_overview_usage_hit_rate({
                rate: (usageStore.totals.cache_hit_rate * 100).toFixed(1),
              })}
            </div>
          </div>
        {/if}
        <p class="text-xs opacity-50">{manage_overview_usage_scope_hint()}</p>
      </div>

      <!-- Quick actions -->
      <div class="card preset-tonal-surface p-5 space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_overview_quick_actions()}
        </h2>
        <div class="flex flex-wrap gap-2">
          {#if !budgetStore.unlimited}
            <button
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
              onclick={() => budgetStore.adjust(100_000_000).catch(() => {})}
              disabled={budgetStore.isBusy}
              >{manage_overview_100m_budget()}</button
            >
          {/if}
          {#if !budgetStore.isPaused}
            <button
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
              onclick={() => (showPauseDialog = true)}
              disabled={budgetStore.isBusy}>{manage_budget_clear()}</button
            >
          {/if}
        </div>
      </div>

      <!-- Links -->
      <div class="card preset-tonal-surface p-5 space-y-2">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_overview_configuration()}
        </h2>
        <div class="flex flex-col gap-1 text-sm">
          <a
            href={`${basePath}/profiles`}
            class="hover:underline opacity-80 hover:opacity-100"
            >{manage_overview_profiles_link()}</a
          >
          <a
            href={`${basePath}/schedules`}
            class="hover:underline opacity-80 hover:opacity-100"
            >{manage_overview_schedules_link()}</a
          >
        </div>
      </div>
    </div>
  </div>
</div>

<ConfirmDialog
  busy={budgetStore.isBusy}
  open={showPauseDialog}
  title={manage_budget_clear()}
  description={manage_budget_clear_desc()}
  confirmLabel={common_clear()}
  tone="danger"
  onConfirm={onConfirmPause}
  onCancel={() => (showPauseDialog = false)}
/>
