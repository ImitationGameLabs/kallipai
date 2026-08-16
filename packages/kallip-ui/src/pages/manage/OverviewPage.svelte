<script lang="ts">
  import { budgetStore } from "../../lib/manage/budget.svelte.ts";
  import { agentsStore } from "../../lib/manage/agents.svelte.ts";
  import { formatTokenCount } from "../../lib/tagmata.svelte.ts";
  import BudgetBar from "../../components/manage/BudgetBar.svelte";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";

  $effect(() => {
    budgetStore.startPolling(5000);
    agentsStore.startPolling(5000);
    return () => { budgetStore.stopPolling(); agentsStore.stopPolling(); };
  });

  let { basePath = "/local/manage" }: { basePath?: string } = $props();
  let showPauseDialog = $state(false);

  async function onConfirmPause() {
    try { await budgetStore.pauseAll(); showPauseDialog = false; } catch {}
  }
</script>

<svelte:head><title>KallipAI · overview</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-2xl space-y-6">
    <h1 class="text-xl font-semibold">Overview</h1>

    {#if budgetStore.error}
      <p class="text-error-500 dark:text-error-400 text-sm">{budgetStore.error}</p>
    {/if}

    <div class="grid grid-cols-1 sm:grid-cols-2 gap-4">
      <!-- Budget -->
      <a href={`${basePath}/budget`} class="card preset-tonal-surface p-5 space-y-2 hover:preset-filled-surface-400 transition">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">Budget</h2>
        <BudgetBar consumed={budgetStore.consumed} budget={budgetStore.budget} />
        <div class="text-sm">
          {budgetStore.consumedPct}% consumed · {formatTokenCount(budgetStore.remaining)} remaining
        </div>
      </a>

      <!-- Agents -->
      <a href={`${basePath}/agents`} class="card preset-tonal-surface p-5 space-y-2 hover:preset-filled-surface-400 transition">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">Agents</h2>
        <div class="text-sm space-y-1">
          <div>{agentsStore.idleCount} idle</div>
          <div>{agentsStore.busyCount} busy</div>
          {#if agentsStore.faultedCount > 0}
            <div class="text-error-500 dark:text-error-400">{agentsStore.faultedCount} faulted</div>
          {/if}
        </div>
      </a>

      <!-- Quick actions -->
      <div class="card preset-tonal-surface p-5 space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">Quick Actions</h2>
        <div class="flex flex-wrap gap-2">
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
            onclick={() => budgetStore.adjust(100_000_000).catch(() => {})}
            disabled={budgetStore.isBusy}
          >+100M budget</button>
          {#if !budgetStore.isPaused}
            <button
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
              onclick={() => (showPauseDialog = true)}
              disabled={budgetStore.isBusy}
            >Clear budget</button>
          {/if}
        </div>
      </div>

      <!-- Links -->
      <div class="card preset-tonal-surface p-5 space-y-2">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">Configuration</h2>
        <div class="flex flex-col gap-1 text-sm">
          <a href={`${basePath}/profiles`} class="hover:underline opacity-80 hover:opacity-100">Profiles →</a>
          <a href={`${basePath}/schedules`} class="hover:underline opacity-80 hover:opacity-100">Schedules →</a>
        </div>
      </div>
    </div>
  </div>
</div>


<ConfirmDialog
  busy={budgetStore.isBusy}
  open={showPauseDialog}
  title="Clear Budget"
  description="This sets remaining budget to 0. All agents will stop immediately. Use the Set button with a new amount to restore."
  confirmLabel="Clear"
  tone="danger"
  onConfirm={onConfirmPause}
  onCancel={() => (showPauseDialog = false)}
/>
