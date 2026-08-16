<script lang="ts">
  import { SvelteSet } from "svelte/reactivity";
  import { profilesStore } from "../../lib/manage/profiles.svelte.ts";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import {
    common_loading,
    common_hide,
    common_show,
    manage_profiles_title,
    manage_profiles_heading,
    manage_profiles_save_changes,
    manage_profiles_apply_all,
    manage_profiles_discard,
    manage_profiles_applied_result,
    manage_profiles_tiers_hazard,
    manage_profiles_tiers,
    manage_profiles_tier,
    manage_profiles_id_placeholder,
    manage_profiles_model_placeholder,
    manage_profiles_max_context_placeholder,
    manage_profiles_remove_profile,
    manage_profiles_add_profile,
    manage_profiles_add_tier,
    manage_profiles_remove_last_tier,
    manage_profiles_endpoints,
    manage_profiles_base_url_placeholder,
    manage_profiles_api_key_placeholder,
    manage_profiles_current_key,
    manage_profiles_remove_endpoint,
    manage_profiles_add_endpoint,
    manage_profiles_apply_title,
    manage_profiles_apply_desc,
    manage_profiles_apply,
  } from "../../paraglide/messages.js";

  // Track which endpoints show their API key in plaintext. Uses type="text"
  // (not "password") so browser password managers never fire for API keys.
  let showKeys = new SvelteSet<string>();

  let { basePath = "/local/manage" }: { basePath?: string } = $props();
  $effect(() => {
    profilesStore.refresh();
  });

  let showApplyDialog = $state(false);
  let applyResult = $state<string | null>(null);

  async function onApply() {
    applyResult = null;
    try {
      const r = await profilesStore.apply();
      showApplyDialog = false;
      applyResult = manage_profiles_applied_result({
        applied: r.applied,
        skipped: r.skipped,
      });
    } catch {
      // Error surfaced via store
    }
  }

  async function onSave() {
    await profilesStore.save().catch(() => {});
  }

  function apiKeyMasked(key: string): string {
    if (key.length <= 4) return "••••";
    return "••••••••" + key.slice(-4);
  }
</script>

<svelte:head><title>{manage_profiles_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-2xl space-y-6">
    <div class="flex items-center justify-between">
      <h1 class="text-xl font-semibold">{manage_profiles_heading()}</h1>
      <div class="flex gap-2">
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
          onclick={() => profilesStore.refresh()}>⟳</button
        >
        <button
          class="btn btn-sm preset-filled-primary-500"
          disabled={!profilesStore.isDirty || profilesStore.isSaving}
          onclick={onSave}
          >{profilesStore.isSaving ? "…" : manage_profiles_save_changes()}</button
        >
        <button
          class="btn btn-sm preset-filled-secondary-500"
          disabled={profilesStore.isDirty || profilesStore.isSaving}
          onclick={() => (showApplyDialog = true)}>{manage_profiles_apply_all()}</button
        >
      </div>
    </div>

    {#if profilesStore.error}
      <p class="text-error-500 dark:text-error-400 text-sm">
        {profilesStore.error}
      </p>
    {/if}
    {#if applyResult}
      <p class="text-success-500 dark:text-success-400 text-sm">
        {applyResult}
      </p>
    {/if}
    {#if profilesStore.isDirty}
      <button
        class="text-xs opacity-60 hover:opacity-100"
        onclick={() => profilesStore.reset()}
      >
        {manage_profiles_discard()}
      </button>
    {/if}

    {#if profilesStore.isLoading}
      <p class="opacity-60 text-sm">{common_loading()}</p>
    {/if}

    {#if profilesStore.draft}
      <!-- Tier hazard warning -->
      <div
        class="card preset-tonal-surface p-3 text-xs opacity-70 border-l-4 border-l-warning-500"
      >
        ⚠ {manage_profiles_tiers_hazard()}
      </div>

      <!-- Tiers -->
      <section class="card preset-tonal-surface p-5 space-y-4">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_profiles_tiers()}
        </h2>
        {#each profilesStore.draft.tiers as tier, tierIdx (tierIdx)}
          <div class="border-l-2 border-l-surface-300 pl-4 space-y-2">
            <div class="text-xs font-medium">
              {manage_profiles_tier({ index: tierIdx })}
            </div>
            {#each tier.profiles as profile, profileIdx (profileIdx)}
              <div class="grid grid-cols-2 gap-2 text-sm">
                <input
                  class="input"
                  placeholder={manage_profiles_id_placeholder()}
                  bind:value={profile.id}
                />
                <select class="select" bind:value={profile.endpoint}>
                  {#each Object.keys(profilesStore.draft.endpoints) as epId}
                    <option value={epId}>{epId}</option>
                  {/each}
                </select>
                <input
                  class="input"
                  placeholder={manage_profiles_model_placeholder()}
                  bind:value={profile.model}
                />
                <input
                  type="number"
                  class="input"
                  placeholder={manage_profiles_max_context_placeholder()}
                  bind:value={profile.max_context_window}
                />
                <button
                  class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500 col-span-2"
                  onclick={() =>
                    profilesStore.removeProfile(tierIdx, profileIdx)}
                  >{manage_profiles_remove_profile()}</button
                >
              </div>
            {/each}
            <button
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
              onclick={() => profilesStore.addProfile(tierIdx)}
              >{manage_profiles_add_profile()}</button
            >
          </div>
        {/each}
        <div class="flex gap-2 pt-2">
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
            onclick={() => profilesStore.addTier()}>{manage_profiles_add_tier()}</button
          >
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
            disabled={profilesStore.draft.tiers.length === 0}
            onclick={() => profilesStore.removeLastTier()}
            >{manage_profiles_remove_last_tier()}</button
          >
        </div>
      </section>

      <!-- Endpoints -->
      <section class="card preset-tonal-surface p-5 space-y-4">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_profiles_endpoints()}
        </h2>
        {#each Object.entries(profilesStore.draft.endpoints) as [epId, ep] (epId)}
          <div class="space-y-2 border-l-2 border-l-surface-300 pl-4">
            <div class="text-xs font-medium font-mono">{epId}</div>
            <div class="grid grid-cols-2 gap-2 text-sm">
              <select class="select" bind:value={ep.family}>
                <option value="deepseek">deepseek</option>
                <option value="openai-compatible">openai-compatible</option>
              </select>
              <input
                class="input"
                placeholder={manage_profiles_base_url_placeholder()}
                bind:value={ep.base_url}
              />
              <div class="col-span-2 flex gap-1">
                <input
                  class="input flex-1"
                  type="text"
                  placeholder={manage_profiles_api_key_placeholder()}
                  style="-webkit-text-security: {showKeys.has(epId)
                    ? 'none'
                    : 'disc'}"
                  bind:value={ep.api_key}
                />
                <button
                  type="button"
                  class="btn btn-sm preset-outlined-surface-500 shrink-0"
                  onclick={() =>
                    showKeys.has(epId)
                      ? showKeys.delete(epId)
                      : showKeys.add(epId)}
                  >{showKeys.has(epId) ? common_hide() : common_show()}</button
                >
              </div>
            </div>
            <div class="text-xs opacity-50 font-mono">
              {manage_profiles_current_key({ key: apiKeyMasked(ep.api_key) })}
            </div>
            <button
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
              onclick={() => profilesStore.removeEndpoint(epId)}
              >{manage_profiles_remove_endpoint()}</button
            >
          </div>
        {/each}
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
          onclick={() => profilesStore.addEndpoint()}
          >{manage_profiles_add_endpoint()}</button
        >
      </section>
    {/if}
  </div>
</div>

<ConfirmDialog
  busy={profilesStore.isSaving}
  open={showApplyDialog}
  title={manage_profiles_apply_title()}
  description={manage_profiles_apply_desc()}
  confirmLabel={manage_profiles_apply()}
  tone="primary"
  onConfirm={onApply}
  onCancel={() => (showApplyDialog = false)}
/>
