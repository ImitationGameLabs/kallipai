<script lang="ts">
  import { budgetStore } from "../../lib/manage/budget.svelte.ts";
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import { formatTokenCount } from "../../lib/tagmata.svelte.ts";
  import BudgetBar from "../../components/manage/BudgetBar.svelte";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import {
    manage_overview_title,
    manage_overview_heading,
    manage_budget_heading,
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
  } from "../../paraglide/messages.js";

  $effect(() => {
    budgetStore.startPolling(5000);
    agentsStore.startPolling(5000);
    return () => {
      budgetStore.stopPolling();
      agentsStore.stopPolling();
    };
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
  <div class="p-6 max-w-2xl space-y-6">
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
        />
        <div class="text-sm">
          {manage_overview_budget_consumed_line({
            pct: budgetStore.consumedPct,
            remaining: formatTokenCount(budgetStore.remaining),
          })}
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

      <!-- Quick actions -->
      <div class="card preset-tonal-surface p-5 space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_overview_quick_actions()}
        </h2>
        <div class="flex flex-wrap gap-2">
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
            onclick={() => budgetStore.adjust(100_000_000).catch(() => {})}
            disabled={budgetStore.isBusy}
            >{manage_overview_100m_budget()}</button
          >
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
