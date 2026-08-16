<script lang="ts">
  import { budgetStore } from "../../lib/manage/budget.svelte.ts";
  import { formatTokenCount } from "../../lib/tagmata.svelte.ts";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import BudgetBar from "../../components/manage/BudgetBar.svelte";

  $effect(() => {
    budgetStore.startPolling(5000);
    return () => budgetStore.stopPolling();
  });

  let { basePath = "/local/manage" }: { basePath?: string } = $props();
let showPauseDialog = $state(false);

  // Adjust panel: one input + three action buttons (increase/decrease/set).
  // Unit selector so the user types small numbers (e.g. "50" + "M") instead
  // of raw token counts ("50000000"). Default M matches the quick-adjust buttons.
  let adjustInput = $state("");
  let adjustUnit = $state<"K" | "M" | "B">("M");
  const UNIT_FACTOR: Record<"K" | "M" | "B", number> = { K: 1_000, M: 1_000_000, B: 1_000_000_000 };

  const adjustDisabled = $derived(!adjustInput || isNaN(parseInt(adjustInput, 10)) || budgetStore.isBusy);

  function rawValue(): number {
    const n = Number(adjustInput);
    return Number.isFinite(n) && n >= 0 ? Math.round(n * UNIT_FACTOR[adjustUnit]) : 0;
  }

  async function onIncrease() {
    const val = rawValue();
    if (val <= 0) return;
    try { await budgetStore.adjust(val); adjustInput = ""; } catch {}
  }

  async function onDecrease() {
    const val = rawValue();
    if (val <= 0) return;
    try { await budgetStore.adjust(-val); adjustInput = ""; } catch {}
  }

  async function onSet() {
    const val = rawValue();
    if (val < 0) return;
    try { await budgetStore.setRemaining(val); adjustInput = ""; } catch {}
  }

  async function onPauseAll() {
    try {
      await budgetStore.pauseAll();
      showPauseDialog = false;
    } catch {
      // Error surfaced via store.error
    }
  }

  function fmtRate(rate: number | null): string {
    if (rate === null) return "—";
    if (rate === 0) return "idle";
    return `~${formatTokenCount(rate)}/min`;
  }

  function fmtEta(eta: number | null): string {
    if (eta === null) return "—";
    if (eta === 0) return "<1 min";
    if (eta < 60) return `~${eta} min`;
    const hours = Math.floor(eta / 60);
    return `~${hours}h ${eta % 60}m`;
  }
</script>

<svelte:head><title>KallipAI · budget</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-lg space-y-6">
    <h1 class="text-xl font-semibold">Budget</h1>

    {#if budgetStore.error}
      <p class="text-error-500 dark:text-error-400 text-sm">{budgetStore.error}</p>
    {/if}

    <!-- Progress bar -->
    <section class="card preset-tonal-surface p-5 space-y-4">
      <BudgetBar
        consumed={budgetStore.consumed}
        budget={budgetStore.budget}
      />
      <div class="grid grid-cols-2 gap-4 text-sm">
        <div>
          <div class="opacity-60 text-xs uppercase tracking-wide">Consumed</div>
          <div class="font-medium">
            {formatTokenCount(budgetStore.consumed)} /
            {formatTokenCount(budgetStore.budget)} tokens
          </div>
        </div>
        <div>
          <div class="opacity-60 text-xs uppercase tracking-wide">Remaining</div>
          <div class="font-medium">{formatTokenCount(budgetStore.remaining)} tokens</div>
        </div>
        <div>
          <div class="opacity-60 text-xs uppercase tracking-wide">Burn rate</div>
          <div class="font-medium">{fmtRate(budgetStore.burnRate)}</div>
        </div>
        <div>
          <div class="opacity-60 text-xs uppercase tracking-wide">ETA</div>
          <div class="font-medium">{fmtEta(budgetStore.etaMinutes)}</div>
        </div>
      </div>
    </section>

    <!-- Quick adjust -->
    <section class="card preset-tonal-surface p-5 space-y-3">
      <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
        Quick Adjust
      </h2>
      <div class="flex flex-wrap gap-2">
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
          disabled={budgetStore.isBusy}
          onclick={() => budgetStore.adjust(50_000_000).catch(() => {})}
        >+50M</button>
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
          disabled={budgetStore.isBusy}
          onclick={() => budgetStore.adjust(100_000_000).catch(() => {})}
        >+100M</button>
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
          disabled={budgetStore.isBusy}
          onclick={() => budgetStore.adjust(-50_000_000).catch(() => {})}
        >−50M</button>
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
          disabled={budgetStore.isBusy}
          onclick={() => budgetStore.adjust(-100_000_000).catch(() => {})}
        >−100M</button>
      </div>
    </section>

    <!-- Precise adjust -->
    <section class="card preset-tonal-surface p-5 space-y-3">
      <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
        Adjust Budget
      </h2>
      <div class="flex gap-2">
        <input
          type="number"
          min="0"
          class="input flex-1"
          placeholder="amount"
          bind:value={adjustInput}
          onkeydown={(e) => { if (e.key === "Enter") onIncrease(); }}
        />
        <select class="select w-20" aria-label="Unit" bind:value={adjustUnit}>
          <option value="K">K</option>
          <option value="M">M</option>
          <option value="B">B</option>
        </select>
      </div>
      <div class="flex gap-2">
        <button
          class="btn preset-filled-primary-500 flex-1"
          disabled={adjustDisabled || budgetStore.isBusy}
          onclick={onIncrease}
        >Increase</button>
        <button
          class="btn preset-filled-error-500 flex-1"
          disabled={adjustDisabled || budgetStore.isBusy}
          onclick={onDecrease}
        >Decrease</button>
        <button
          class="btn preset-outlined-surface-500 hover:preset-filled-surface-500 flex-1"
          disabled={adjustDisabled || budgetStore.isBusy}
          onclick={onSet}
        >Set</button>
      </div>
      <p class="text-xs opacity-50">
        Amount is in the selected unit (K = thousand, M = million, B = billion tokens). Increase/decrease adjusts budget by the amount; Set sets the remaining tokens directly (0 = pause all).
      </p>
    </section>

    <!-- Pause / Resume -->
    <section class="card preset-tonal-surface p-5 space-y-3">
      {#if budgetStore.isPaused}
        <div class="flex items-center gap-2 text-sm">
          <span class="size-2 rounded-full bg-error-500" aria-hidden="true"></span>
          <span class="font-medium">Budget cleared</span>
        </div>
        <p class="text-xs opacity-60">
          Remaining budget is 0. All agents are stopped. Enter an amount above to restore.
        </p>
        <div class="flex gap-2">
          <input
            type="number"
            min="0"
            class="input flex-1"
            placeholder="amount to restore"
            bind:value={adjustInput}
          />
          <select class="select w-20" aria-label="Unit" bind:value={adjustUnit}>
            <option value="K">K</option>
            <option value="M">M</option>
            <option value="B">B</option>
          </select>
          <button
            class="btn preset-filled-success-500"
            disabled={adjustDisabled || budgetStore.isBusy}
            onclick={onSet}
          >Set & Resume</button>
        </div>
      {:else}
        <button
          class="btn preset-filled-error-500"
          onclick={() => (showPauseDialog = true)}
        >Clear Budget</button>
        <p class="text-xs opacity-50">
          Sets remaining budget to 0 — all agents will stop immediately.
        </p>
      {/if}
    </section>
  </div>
</div>

<ConfirmDialog
  busy={budgetStore.isBusy}
  open={showPauseDialog}
  title="Clear Budget"
  description="This sets remaining budget to 0. All agents will stop immediately. Use the Set button with a new amount to restore."
  confirmLabel="Clear"
  tone="danger"
  onConfirm={onPauseAll}
  onCancel={() => (showPauseDialog = false)}
/>
