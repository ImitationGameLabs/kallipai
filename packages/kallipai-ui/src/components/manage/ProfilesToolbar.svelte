<script lang="ts">
  // Toolbar card for the profiles page: heading, refresh/test-all/save/apply
  // buttons, the error/result/discard/loading lines, and the advisory banners
  // (set hazard, parked-live). The apply *flow* — dialog open state and the
  // confirm handler — stays in the page so the tail ConfirmDialog keeps a
  // single source of truth; this row only requests it.
  import type { ParkedLiveSnapshot } from "../../lib/manage/parkedLive.ts";
  import type { ProfilesStore } from "../../lib/manage/profiles.svelte.ts";
  import {
    common_loading,
    manage_profiles_apply_all,
    manage_profiles_discard,
    manage_profiles_heading,
    manage_profiles_heading_desc_l1,
    manage_profiles_heading_desc_l2,
    manage_profiles_heading_desc_l3,
    manage_profiles_parking_warn,
    manage_profiles_save_changes,
    manage_profiles_test_all,
    manage_profiles_sets_hazard,
  } from "../../paraglide/messages.js";

  let {
    store,
    applyResult,
    parkedLive,
    onTestAll,
    onSave,
    onDiscard,
    onRequestApply,
  }: {
    store: ProfilesStore;
    applyResult: string | null;
    parkedLive: ParkedLiveSnapshot | null;
    onTestAll: () => void;
    onSave: () => void;
    onDiscard: () => void;
    onRequestApply: () => void;
  } = $props();
</script>

<div class="flex items-center justify-between">
  <div>
    <!-- md+ keeps this h1; below md the shell top row carries the title (AppShell `title`). -->
    <h1 class="text-xl font-semibold hidden md:block">
      {manage_profiles_heading()}
    </h1>
    <div class="text-xs opacity-60 mt-1 space-y-0.5">
      <p>{manage_profiles_heading_desc_l1()}</p>
      <p>{manage_profiles_heading_desc_l2()}</p>
      <p>{manage_profiles_heading_desc_l3()}</p>
    </div>
  </div>
  <div class="flex flex-wrap gap-2">
    <button
      class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
      onclick={() => store.refresh()}>⟳</button
    >
    <button
      class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
      disabled={store.isProbing}
      onclick={onTestAll}
      >{store.isProbing ? "…" : manage_profiles_test_all()}</button
    >
    <button
      class="btn btn-sm preset-filled-primary-500"
      disabled={!store.isDirty || store.isSaving}
      onclick={onSave}
      >{store.isSaving ? "…" : manage_profiles_save_changes()}</button
    >
    <button
      class="btn btn-sm preset-filled-secondary-500"
      disabled={store.isDirty || store.isSaving}
      onclick={onRequestApply}>{manage_profiles_apply_all()}</button
    >
  </div>
</div>

{#if store.error}
  <p class="text-error-500 dark:text-error-400 text-sm">
    {store.error}
  </p>
{/if}
{#if applyResult}
  <p class="text-success-500 dark:text-success-400 text-sm">
    {applyResult}
  </p>
{/if}
{#if store.probeError}
  <p class="text-error-500 dark:text-error-400 text-sm font-mono break-all">
    {store.probeError}
  </p>
{/if}
{#if store.isDirty}
  <button class="text-xs opacity-60 hover:opacity-100" onclick={onDiscard}>
    {manage_profiles_discard()}
  </button>
{/if}

{#if store.isLoading}
  <p class="opacity-60 text-sm">{common_loading()}</p>
{/if}

{#if store.draft}
  <div
    class="card preset-tonal-surface p-3 text-xs opacity-70 border-l-4 border-l-warning-500"
  >
    ⚠ {manage_profiles_sets_hazard()}
  </div>

  {#if parkedLive}
    <div
      class="card preset-tonal-surface p-3 text-xs opacity-70 border-l-4 border-l-warning-500"
    >
      ⚠ {manage_profiles_parking_warn({
        count: parkedLive.agentCount,
        ids: parkedLive.profileIds.join(", "),
      })}
    </div>
  {/if}
{/if}
