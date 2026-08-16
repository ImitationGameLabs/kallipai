<script lang="ts">
  import { budgetStore } from "../../lib/manage/budget.svelte.ts";
  import { formatTokenCount } from "../../lib/tagmata.svelte.ts";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import BudgetBar from "../../components/manage/BudgetBar.svelte";
  import { getLocale } from "../../paraglide/runtime.js";

  import {
    manage_budget_title,
    manage_budget_heading,
    manage_budget_consumed,
    manage_budget_remaining,
    manage_budget_burn_rate,
    manage_budget_eta,
    manage_budget_tokens_of,
    manage_budget_tokens,
    manage_budget_idle,
    manage_budget_rate_per_min,
    manage_budget_eta_under,
    manage_budget_eta_min,
    manage_budget_eta_hm,
    manage_budget_quick_adjust,
    manage_budget_adjust_budget,
    manage_budget_amount,
    manage_budget_unit_aria,
    manage_budget_increase,
    manage_budget_decrease,
    manage_budget_set,
    manage_budget_adjust_hint,
    manage_budget_cleared,
    manage_budget_budget_cleared_hint,
    manage_budget_amount_restore,
    manage_budget_set_resume,
    manage_budget_clear,
    manage_budget_clear_desc,
    common_clear,
    manage_budget_clear_hint,
  } from "../../paraglide/messages.js";
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
  const UNIT_FACTOR: Record<"K" | "M" | "B", number> = {
    K: 1_000,
    M: 1_000_000,
    B: 1_000_000_000,
  };

  const adjustDisabled = $derived(
    !adjustInput || isNaN(parseInt(adjustInput, 10)) || budgetStore.isBusy,
  );

  function rawValue(): number {
    const n = Number(adjustInput);
    return Number.isFinite(n) && n >= 0
      ? Math.round(n * UNIT_FACTOR[adjustUnit])
      : 0;
  }

  async function onIncrease() {
    const val = rawValue();
    if (val <= 0) return;
    try {
      await budgetStore.adjust(val);
      adjustInput = "";
    } catch {}
  }

  async function onDecrease() {
    const val = rawValue();
    if (val <= 0) return;
    try {
      await budgetStore.adjust(-val);
      adjustInput = "";
    } catch {}
  }

  async function onSet() {
    const val = rawValue();
    if (val < 0) return;
    try {
      await budgetStore.setRemaining(val);
      adjustInput = "";
    } catch {}
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
    if (rate === 0) return manage_budget_idle();
    return manage_budget_rate_per_min({ rate: formatTokenCount(rate) });
  }

  function fmtEta(eta: number | null): string {
    if (eta === null) return "—";
    if (eta === 0) return manage_budget_eta_under();
    if (eta < 60) return manage_budget_eta_min({ m: eta });
    const hours = Math.floor(eta / 60);
    return manage_budget_eta_hm({ h: hours, m: eta % 60 });
  }
</script>

<svelte:head><title>{manage_budget_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-lg space-y-6">
    <h1 class="text-xl font-semibold">{manage_budget_heading()}</h1>

    {#if budgetStore.error}
      <p class="text-error-500 dark:text-error-400 text-sm">
        {budgetStore.error}
      </p>
    {/if}

    <!-- Progress bar -->
    <section class="card preset-tonal-surface p-5 space-y-4">
      <BudgetBar consumed={budgetStore.consumed} budget={budgetStore.budget} />
      <div class="grid grid-cols-2 gap-4 text-sm">
        <div>
          <div class="opacity-60 text-xs uppercase tracking-wide">
            {manage_budget_consumed()}
          </div>
          <div class="font-medium">
            {manage_budget_tokens_of({
              consumed: formatTokenCount(budgetStore.consumed),
              total: formatTokenCount(budgetStore.budget),
            })}
          </div>
        </div>
        <div>
          <div class="opacity-60 text-xs uppercase tracking-wide">
            {manage_budget_remaining()}
          </div>
          <div class="font-medium">
            {manage_budget_tokens({ count: formatTokenCount(budgetStore.remaining) })}
          </div>
        </div>
        <div>
          <div class="opacity-60 text-xs uppercase tracking-wide">
            {manage_budget_burn_rate()}
          </div>
          <div class="font-medium">{fmtRate(budgetStore.burnRate)}</div>
        </div>
        <div>
          <div class="opacity-60 text-xs uppercase tracking-wide">
            {manage_budget_eta()}
          </div>
          <div class="font-medium">{fmtEta(budgetStore.etaMinutes)}</div>
        </div>
      </div>
    </section>

    <!-- Quick adjust -->
    <section class="card preset-tonal-surface p-5 space-y-3">
      <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
        {manage_budget_quick_adjust()}
      </h2>
      <div class="flex flex-wrap gap-2">
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
          disabled={budgetStore.isBusy}
          onclick={() => budgetStore.adjust(50_000_000).catch(() => {})}
          >+50M</button
        >
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
          disabled={budgetStore.isBusy}
          onclick={() => budgetStore.adjust(100_000_000).catch(() => {})}
          >+100M</button
        >
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
          disabled={budgetStore.isBusy}
          onclick={() => budgetStore.adjust(-50_000_000).catch(() => {})}
          >−50M</button
        >
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
          disabled={budgetStore.isBusy}
          onclick={() => budgetStore.adjust(-100_000_000).catch(() => {})}
          >−100M</button
        >
      </div>
    </section>

    <!-- Precise adjust -->
    <section class="card preset-tonal-surface p-5 space-y-3">
      <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
        {manage_budget_adjust_budget()}
      </h2>
      <div class="flex gap-2">
        <input
          type="number"
          min="0"
          class="input flex-1"
          placeholder={manage_budget_amount()}
          bind:value={adjustInput}
          onkeydown={(e) => {
            if (e.key === "Enter") onIncrease();
          }}
        />
        <select
          class="select w-20"
          aria-label={manage_budget_unit_aria()}
          bind:value={adjustUnit}
        >
          <option value="K">K</option>
          <option value="M">M</option>
          <option value="B">B</option>
        </select>
      </div>
      <div class="flex gap-2">
        <button
          class="btn preset-filled-primary-500 flex-1"
          disabled={adjustDisabled || budgetStore.isBusy}
          onclick={onIncrease}>{manage_budget_increase()}</button
        >
        <button
          class="btn preset-filled-error-500 flex-1"
          disabled={adjustDisabled || budgetStore.isBusy}
          onclick={onDecrease}>{manage_budget_decrease()}</button
        >
        <button
          class="btn preset-outlined-surface-500 hover:preset-filled-surface-500 flex-1"
          disabled={adjustDisabled || budgetStore.isBusy}
          onclick={onSet}>{manage_budget_set()}</button
        >
      </div>
      <p class="text-xs opacity-50">
        {manage_budget_adjust_hint()}
      </p>
    </section>

    <!-- Pause / Resume -->
    <section class="card preset-tonal-surface p-5 space-y-3">
      {#if budgetStore.isPaused}
        <div class="flex items-center gap-2 text-sm">
          <span class="size-2 rounded-full bg-error-500" aria-hidden="true"
          ></span>
          <span class="font-medium">{manage_budget_cleared()}</span>
        </div>
        <p class="text-xs opacity-60">
          {manage_budget_budget_cleared_hint()}
        </p>
        <div class="flex gap-2">
          <input
            type="number"
            min="0"
            class="input flex-1"
            placeholder={manage_budget_amount_restore()}
            bind:value={adjustInput}
          />
          <select
            class="select w-20"
            aria-label={manage_budget_unit_aria()}
            bind:value={adjustUnit}
          >
            <option value="K">K</option>
            <option value="M">M</option>
            <option value="B">B</option>
          </select>
          <button
            class="btn preset-filled-success-500"
            disabled={adjustDisabled || budgetStore.isBusy}
            onclick={onSet}>{manage_budget_set_resume()}</button
          >
        </div>
      {:else}
        <button
          class="btn preset-filled-error-500"
          onclick={() => (showPauseDialog = true)}
          >{manage_budget_clear()}</button
        >
        <p class="text-xs opacity-50">
          {manage_budget_clear_hint()}
        </p>
      {/if}
    </section>
  </div>
</div>

<ConfirmDialog
  busy={budgetStore.isBusy}
  open={showPauseDialog}
  title={manage_budget_clear()}
  description={manage_budget_clear_desc()}
  confirmLabel={common_clear()}
  tone="danger"
  onConfirm={onPauseAll}
  onCancel={() => (showPauseDialog = false)}
/>
