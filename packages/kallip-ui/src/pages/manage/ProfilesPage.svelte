<script lang="ts">
  // Profiles manage page — card-based three-layer view (provider cards in a
  // global pool, tier containers holding profile cards), matching the wire
  // shape 1:1. Read-mostly: editing goes through the Provider/Tier dialogs;
  // profile cards drag between tiers (HTML5 DnD updating the draft).
  //
  // Probe results route inline to the card that triggered them: the page
  // accumulates providerReports/profileReports maps keyed by id, because the
  // store's single `probe` field is replaced wholesale on every call.
  import { profilesStore } from "../../lib/manage/profiles.svelte.ts";
  import { SvelteMap } from "svelte/reactivity";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    FlaskConical,
    MoreVertical,
    Pencil,
    Plus,
    Trash,
  } from "@lucide/svelte";
  import ProviderDialog from "../../components/manage/ProviderDialog.svelte";
  import TierDialog from "../../components/manage/TierDialog.svelte";
  import {
    moveProfile,
    replaceTierProfiles,
    singleProfileProbeRequest,
    upsertProvider,
  } from "../../lib/manage/compute.ts";
  import type {
    ProfileProvider,
    ProfileProviderProbeReport,
    ProfileModelProbeReport,
    ProfileProbeRequest,
    ProfileProbeResponse,
    ProfileProbeStatus,
  } from "@kallipai/kallip-client";
  import {
    common_edit,
    common_remove,
    common_loading,
    manage_profiles_add_provider,
    manage_profiles_add_tier,
    manage_profiles_apply,
    manage_profiles_apply_all,
    manage_profiles_apply_desc,
    manage_profiles_apply_title,
    manage_profiles_applied_result,
    manage_profiles_discard,
    manage_profiles_provider_base_url_default,
    manage_profiles_provider_card_base_url_label,
    manage_profiles_provider_actions_aria,
    manage_profiles_providers,
    manage_profiles_heading,
    manage_profiles_heading_desc_l1,
    manage_profiles_heading_desc_l2,
    manage_profiles_heading_desc_l3,
    manage_profiles_max_context_label,
    manage_profiles_probe_models_one,
    manage_profiles_probe_models_other,
    manage_profiles_probe_request_failed,
    manage_profiles_probe_status_invalid,
    manage_profiles_probe_status_ok,
    manage_profiles_probe_status_partial,
    manage_profiles_probe_status_unauthorized,
    manage_profiles_probe_status_unreachable,
    manage_profiles_probe_tier_ok,
    manage_profiles_probe_tier_fail,
    manage_profiles_profile_provider_label,
    manage_profiles_profile_actions_aria,
    manage_profiles_profile_model_label,
    manage_profiles_remove_tier_confirm_desc,
    manage_profiles_remove_tier_confirm_title,
    manage_profiles_save_changes,
    manage_profiles_test,
    manage_profiles_test_all,
    manage_profiles_tier,
    manage_profiles_tier_drop_here,
    manage_profiles_tier_actions_aria,
    manage_profiles_tiers,
    manage_profiles_tiers_desc_l1,
    manage_profiles_tiers_desc_l2,
    manage_profiles_tiers_desc_l3,
    manage_profiles_tiers_desc_l4,
    manage_profiles_tiers_hazard,
    manage_profiles_title,
  } from "../../paraglide/messages.js";

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
    providerReports.clear();
    profileReports.clear();
  }

  // --- inline probe results, routed by call-site scope ---

  const providerReports = new SvelteMap<string, ProfileProviderProbeReport>();
  const profileReports = new SvelteMap<string, ProfileModelProbeReport>();

  /** `${tierIdx}:${profileId}` — profile ids can repeat across tiers. */
  function profileKey(tierIdx: number, profileId: string): string {
    return `${tierIdx}:${profileId}`;
  }

  function mergeProviderScope(resp: ProfileProbeResponse): void {
    for (const r of resp.results) providerReports.set(r.endpoint_id, r);
  }

  function mergeProfileScope(
    tierIdx: number,
    resp: ProfileProbeResponse,
  ): void {
    // A tier-scoped response's tiers[0] is the requested tier.
    const t = resp.tiers[0];
    if (!t) return;
    for (const p of t.profiles) {
      profileReports.set(profileKey(tierIdx, p.profile_id), p);
    }
  }
  /**
   * Merge an all-scope response: response tiers line up 1:1 with the draft
   * tiers by request order — tiers[i] reports on draft tier i.
   */
  function mergeProfileScopeAll(resp: ProfileProbeResponse): void {
    for (const t of resp.tiers) {
      for (const p of t.profiles) {
        profileReports.set(profileKey(t.index, p.profile_id), p);
      }
    }
  }

  function clearProfileResult(tierIdx: number, profileId: string): void {
    profileReports.delete(profileKey(tierIdx, profileId));
  }

  async function onTestProvider(id: string) {
    await profilesStore.probeProvider(id);
    if (profilesStore.probe) mergeProviderScope(profilesStore.probe);
  }

  async function onTestTier(tierIdx: number) {
    await profilesStore.probeTier(tierIdx);
    if (profilesStore.probe) mergeProfileScope(tierIdx, profilesStore.probe);
  }

  async function onTestProfile(tierIdx: number, profileIdx: number) {
    const draft = profilesStore.draft;
    if (!draft) return;
    const body = singleProfileProbeRequest(
      profilesStore.config,
      draft,
      tierIdx,
      profileIdx,
    );
    if (!body) return;
    const resp = await profilesStore.probeRaw(body);
    if (!resp) return;
    mergeProviderScope(resp);
    mergeProfileScope(tierIdx, resp);
  }

  async function onTestAll() {
    await profilesStore.probeAll();
    if (!profilesStore.probe) return;
    mergeProviderScope(profilesStore.probe);
    mergeProfileScopeAll(profilesStore.probe);
  }

  function onDiscard() {
    profilesStore.reset();
    providerReports.clear();
    profileReports.clear();
  }

  // --- drag & drop (profile cards between tiers) ---

  interface DragPayload {
    fromTier: number;
    fromIdx: number;
  }

  let drag = $state<DragPayload | null>(null);
  let dragOverTier = $state(-1);

  function onDrop(toTier: number): void {
    const d = drag;
    drag = null;
    dragOverTier = -1;
    const draft = profilesStore.draft;
    if (!d || !draft) return;
    if (d.fromTier !== toTier) {
      // Cross-tier: the key is tier-scoped, so clear the stale source entry
      // (same-tier keeps its key — the report survives the reorder).
      const id = draft.tiers[d.fromTier]?.profiles[d.fromIdx]?.id;
      if (id) clearProfileResult(d.fromTier, id);
    }
    profilesStore.draft = moveProfile(draft, d.fromTier, d.fromIdx, toTier);
  }

  // --- dialogs ---

  let providerDialog = $state<{
    open: boolean;
    mode: "new" | "edit";
    provider: ProfileProvider | null;
  }>({ open: false, mode: "new", provider: null });

  function openProviderNew() {
    providerDialog = { open: true, mode: "new", provider: null };
  }

  function openProviderEdit(ep: ProfileProvider) {
    providerDialog = { open: true, mode: "edit", provider: ep };
  }

  function onProviderSave(result: {
    id: string;
    family: string;
    baseUrl: string | null;
    apiKey: string | null;
  }) {
    const draft = profilesStore.draft;
    if (!draft) return;
    const existing =
      result.apiKey === null
        ? (draft.endpoints[result.id]?.api_key ?? "")
        : result.apiKey;
    profilesStore.draft = upsertProvider(draft, {
      id: result.id,
      family: result.family,
      api_key: existing,
      base_url: result.baseUrl,
    });
    providerDialog.open = false;
  }

  function onProviderRemove() {
    if (providerDialog.mode === "edit" && providerDialog.provider) {
      profilesStore.removeProvider(providerDialog.provider.id);
      providerReports.delete(providerDialog.provider.id);
    }
    providerDialog.open = false;
  }

  // Tier removal confirm: every removal rebinds agents (positional tiers),
  // so the kebab Remove always opens a confirm before mutating the draft.
  let removeTierIdx = $state<number | null>(null);

  function onTierRemoveConfirmed() {
    if (removeTierIdx === null) return;
    profilesStore.removeTier(removeTierIdx);
    profileReports.clear();
    removeTierIdx = null;
  }
  let tierDialog = $state<{ open: boolean; tierIdx: number }>({
    open: false,
    tierIdx: 0,
  });

  function onTierSave(
    rows: {
      id: string;
      endpoint: string;
      model: string;
      max_context_window: number;
    }[],
  ) {
    const draft = profilesStore.draft;
    if (!draft) return;
    profilesStore.draft = replaceTierProfiles(draft, tierDialog.tierIdx, rows);
    tierDialog.open = false;
  }

  // --- display helpers ---

  function probeStatusLabel(s: ProfileProbeStatus): string {
    switch (s) {
      case "ok":
        return manage_profiles_probe_status_ok();
      case "partial":
        return manage_profiles_probe_status_partial();
      case "unreachable":
        return manage_profiles_probe_status_unreachable();
      case "unauthorized":
        return manage_profiles_probe_status_unauthorized();
      case "invalid_config":
        return manage_profiles_probe_status_invalid();
    }
  }

  const probeStatusColor: Record<ProfileProbeStatus, string> = {
    ok: "text-success-500 dark:text-success-400",
    partial: "text-warning-500 dark:text-warning-400",
    unreachable: "text-error-500 dark:text-error-400",
    unauthorized: "text-error-500 dark:text-error-400",
    invalid_config: "text-error-500 dark:text-error-400",
  };

  function modelsCountLabel(count: number): string {
    return count === 1
      ? manage_profiles_probe_models_one({ count })
      : manage_profiles_probe_models_other({ count });
  }

  const providerIds = $derived(
    Object.keys(profilesStore.draft?.endpoints ?? {}),
  );
</script>

<svelte:head><title>{manage_profiles_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-3xl space-y-6">
    <div class="flex items-center justify-between">
      <div>
        <h1 class="text-xl font-semibold">{manage_profiles_heading()}</h1>
        <div class="text-xs opacity-60 mt-1 space-y-0.5">
          <p>{manage_profiles_heading_desc_l1()}</p>
          <p>{manage_profiles_heading_desc_l2()}</p>
          <p>{manage_profiles_heading_desc_l3()}</p>
        </div>
      </div>
      <div class="flex gap-2">
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
          onclick={() => profilesStore.refresh()}>⟳</button
        >
        <button
          class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
          disabled={profilesStore.isProbing}
          onclick={onTestAll}
          >{profilesStore.isProbing ? "…" : manage_profiles_test_all()}</button
        >
        <button
          class="btn btn-sm preset-filled-primary-500"
          disabled={!profilesStore.isDirty || profilesStore.isSaving}
          onclick={onSave}
          >{profilesStore.isSaving
            ? "…"
            : manage_profiles_save_changes()}</button
        >
        <button
          class="btn btn-sm preset-filled-secondary-500"
          disabled={profilesStore.isDirty || profilesStore.isSaving}
          onclick={() => (showApplyDialog = true)}
          >{manage_profiles_apply_all()}</button
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
    {#if profilesStore.probeError}
      <p class="text-error-500 dark:text-error-400 text-sm font-mono break-all">
        {manage_profiles_probe_request_failed({
          error: profilesStore.probeError,
        })}
      </p>
    {/if}
    {#if profilesStore.isDirty}
      <button class="text-xs opacity-60 hover:opacity-100" onclick={onDiscard}>
        {manage_profiles_discard()}
      </button>
    {/if}

    {#if profilesStore.isLoading}
      <p class="opacity-60 text-sm">{common_loading()}</p>
    {/if}

    {#if profilesStore.draft}
      <div
        class="card preset-tonal-surface p-3 text-xs opacity-70 border-l-4 border-l-warning-500"
      >
        ⚠ {manage_profiles_tiers_hazard()}
      </div>

      <!-- Providers: global pool of provider cards -->
      <section class="space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_profiles_providers()}
        </h2>
        <div class="grid gap-3 sm:grid-cols-2">
          {#each Object.values(profilesStore.draft.endpoints) as ep (ep.id)}
            {@const report = providerReports.get(ep.id)}
            <div class="card preset-tonal-surface p-4 space-y-2">
              <div class="flex items-center justify-between gap-2">
                <span class="font-mono text-sm font-semibold">{ep.id}</span>
                <Menu
                  positioning={{ placement: "bottom-end" }}
                  onSelect={(e) => {
                    if (e.value === "test") onTestProvider(ep.id);
                    else if (e.value === "edit") openProviderEdit(ep);
                  }}
                >
                  <Menu.Trigger
                    class="size-8 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-surface-500"
                    aria-label={manage_profiles_provider_actions_aria()}
                    disabled={profilesStore.isProbing}
                  >
                    <MoreVertical class="size-4" />
                  </Menu.Trigger>
                  <Portal>
                    <Menu.Positioner>
                      <Menu.Content
                        class="card preset-tonal-surface p-1 min-w-[8rem]"
                      >
                        <Menu.Item
                          value="test"
                          class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                        >
                          <FlaskConical class="size-4" />
                          {manage_profiles_test()}
                        </Menu.Item>
                        <Menu.Item
                          value="edit"
                          class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                        >
                          <Pencil class="size-4" />
                          {common_edit()}
                        </Menu.Item>
                      </Menu.Content>
                    </Menu.Positioner>
                  </Portal>
                </Menu>
              </div>
              <dl class="text-xs space-y-1">
                <div class="flex gap-2">
                  <dt class="opacity-60">
                    {manage_profiles_profile_provider_label()}:
                  </dt>
                  <dd class="font-mono">{ep.family}</dd>
                </div>
                <div class="flex gap-2">
                  <dt class="opacity-60">
                    {manage_profiles_provider_card_base_url_label()}:
                  </dt>
                  <dd class="font-mono truncate">
                    {ep.base_url ?? manage_profiles_provider_base_url_default()}
                  </dd>
                </div>
                <div class="flex gap-2">
                  <dt class="opacity-60">API key:</dt>
                  <dd class="font-mono">{ep.api_key}</dd>
                </div>
              </dl>
              {#if report}
                <div class="border-t border-surface-300 pt-2 text-xs space-y-1">
                  <span class={probeStatusColor[report.status]}>
                    {probeStatusLabel(report.status)}
                  </span>
                  {#if report.latency_ms != null}
                    <span class="opacity-60 ml-2">{report.latency_ms}ms</span>
                  {/if}
                  {#if report.catalog_count != null}
                    <span class="opacity-60 ml-2">
                      {modelsCountLabel(report.catalog_count)}
                    </span>
                  {/if}
                  {#if report.detail}
                    <p class="opacity-60 font-mono break-all">
                      {report.detail}
                    </p>
                  {/if}
                </div>
              {/if}
            </div>
          {/each}
          <button
            type="button"
            class="card preset-tonal-surface border-2 border-dashed border-surface-400 p-4 flex items-center justify-center gap-2 min-h-24 hover:preset-filled-surface-100-900 transition cursor-pointer"
            onclick={openProviderNew}
          >
            <Plus class="size-6 opacity-70" />
            <span class="text-sm opacity-70">
              {manage_profiles_add_provider()}
            </span>
          </button>
        </div>
      </section>

      <!-- Tiers: one container per tier holding draggable profile cards -->
      <section class="space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_profiles_tiers()}
        </h2>
        <div class="text-xs opacity-60 mt-1 space-y-0.5">
          <p>{manage_profiles_tiers_desc_l1()}</p>
          <p>{manage_profiles_tiers_desc_l2()}</p>
          <p>{manage_profiles_tiers_desc_l3()}</p>
          <p>{manage_profiles_tiers_desc_l4()}</p>
        </div>

        {#each profilesStore.draft.tiers as tier, tierIdx (tierIdx)}
          {@const tierReport = [...profileReports.entries()]
            .filter(([k]) => k.startsWith(`${tierIdx}:`))
            .map(([, v]) => v)}
          <div
            role="list"
            class="card preset-tonal-surface p-4 space-y-3 {dragOverTier ===
            tierIdx
              ? 'outline-2 outline-dashed outline-primary-500'
              : ''}"
            ondragover={(e) => {
              e.preventDefault();
              dragOverTier = tierIdx;
            }}
            ondragleave={() =>
              (dragOverTier = tierIdx === dragOverTier ? -1 : dragOverTier)}
            ondrop={(e) => {
              e.preventDefault();
              onDrop(tierIdx);
            }}
          >
            <div class="flex items-center justify-between gap-2">
              <div class="text-sm font-medium">
                {manage_profiles_tier()}
                <span class="font-mono opacity-80">#{tierIdx}</span>
              </div>
              <Menu
                positioning={{ placement: "bottom-end" }}
                onSelect={(e) => {
                  if (e.value === "test") onTestTier(tierIdx);
                  else if (e.value === "edit")
                    tierDialog = { open: true, tierIdx };
                  else if (e.value === "remove") removeTierIdx = tierIdx;
                }}
              >
                <Menu.Trigger
                  class="size-8 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-surface-500"
                  aria-label={manage_profiles_tier_actions_aria()}
                  disabled={profilesStore.isProbing}
                >
                  <MoreVertical class="size-4" />
                </Menu.Trigger>
                <Portal>
                  <Menu.Positioner>
                    <Menu.Content
                      class="card preset-tonal-surface p-1 min-w-[8rem]"
                    >
                      <Menu.Item
                        value="test"
                        class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                      >
                        <FlaskConical class="size-4" />
                        {manage_profiles_test_all()}
                      </Menu.Item>
                      <Menu.Item
                        value="edit"
                        class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                      >
                        <Pencil class="size-4" />
                        {common_edit()}
                      </Menu.Item>
                      <Menu.Item
                        value="remove"
                        class="flex items-center gap-2 px-3 py-2 rounded-base text-sm text-error-500 dark:text-error-400 cursor-pointer hover:preset-filled-error-500"
                      >
                        <Trash class="size-4" />
                        {common_remove()}
                      </Menu.Item>
                    </Menu.Content>
                  </Menu.Positioner>
                </Portal>
              </Menu>
            </div>

            {#each tier.profiles as profile, profileIdx (profileIdx)}
              {@const report = profileReports.get(
                profileKey(tierIdx, profile.id),
              )}
              <div
                role="listitem"
                class="card preset-filled-surface-100-900 p-3 space-y-1 cursor-grab"
                draggable="true"
                ondragstart={(e) => {
                  drag = { fromTier: tierIdx, fromIdx: profileIdx };
                  // Firefox only starts a drag session if dataTransfer gets data.
                  if (e.dataTransfer) {
                    e.dataTransfer.setData("text/plain", profile.id);
                    e.dataTransfer.effectAllowed = "move";
                  }
                }}
                ondragend={() => {
                  drag = null;
                  dragOverTier = -1;
                }}
              >
                <div class="flex items-center justify-between gap-2">
                  <span class="font-mono text-sm">{profile.id}</span>
                  <Menu
                    positioning={{ placement: "bottom-end" }}
                    onSelect={(e) => {
                      if (e.value === "test")
                        onTestProfile(tierIdx, profileIdx);
                      else if (e.value === "edit")
                        tierDialog = { open: true, tierIdx };
                    }}
                  >
                    <Menu.Trigger
                      class="size-8 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-surface-500"
                      aria-label={manage_profiles_profile_actions_aria()}
                      disabled={profilesStore.isProbing}
                    >
                      <MoreVertical class="size-4" />
                    </Menu.Trigger>
                    <Portal>
                      <Menu.Positioner>
                        <Menu.Content
                          class="card preset-tonal-surface p-1 min-w-[8rem]"
                        >
                          <Menu.Item
                            value="test"
                            class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                          >
                            <FlaskConical class="size-4" />
                            {manage_profiles_test()}
                          </Menu.Item>
                          <Menu.Item
                            value="edit"
                            class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                          >
                            <Pencil class="size-4" />
                            {common_edit()}
                          </Menu.Item>
                        </Menu.Content>
                      </Menu.Positioner>
                    </Portal>
                  </Menu>
                </div>
                <dl class="text-xs space-y-0.5">
                  <div class="flex gap-2">
                    <dt class="opacity-60">
                      {manage_profiles_profile_provider_label()}:
                    </dt>
                    <dd class="font-mono">{profile.endpoint}</dd>
                  </div>
                  <div class="flex gap-2">
                    <dt class="opacity-60">
                      {manage_profiles_profile_model_label()}:
                    </dt>
                    <dd class="font-mono">{profile.model}</dd>
                  </div>
                  <div class="flex gap-2">
                    <dt class="opacity-60">
                      {manage_profiles_max_context_label()}:
                    </dt>
                    <dd class="font-mono">{profile.max_context_window}</dd>
                  </div>
                </dl>
                {#if report}
                  <div class="text-xs">
                    <span class={probeStatusColor[report.status]}>
                      {probeStatusLabel(report.status)}
                    </span>
                    {#if report.detail}
                      <span class="opacity-60 ml-2 font-mono break-all">
                        {report.detail}
                      </span>
                    {/if}
                  </div>
                {/if}
              </div>
            {/each}
            {#if tier.profiles.length === 0}
              <p class="text-xs opacity-50">
                {manage_profiles_tier_drop_here()}
              </p>
            {/if}

            <!-- Card footer: the tier probe summary (a result lands beside
                 the kebab menu that produced it). -->
            {#if tierReport.length > 0}
              <div class="flex items-center gap-2 flex-wrap text-xs">
                {#if tierReport.every((r) => r.status === "ok")}
                  <span class={probeStatusColor.ok}>
                    {manage_profiles_probe_tier_ok()}
                  </span>
                {:else}
                  <span class={probeStatusColor.invalid_config}>
                    {manage_profiles_probe_tier_fail()}
                    {tierReport
                      .filter((r) => r.status !== "ok")
                      .map((r) => r.profile_id)
                      .join(", ")}
                  </span>
                {/if}
              </div>
            {/if}
          </div>
        {/each}

        <!-- Add-tier card: same level as the tier containers; appends an
             empty tier directly (no dialog) — drag profiles in or use a
             tier's Edit. An empty tier cannot be saved (PUT rejects), by
             design: fill it before saving. -->
        <button
          type="button"
          class="card preset-tonal-surface border-2 border-dashed border-surface-400 p-4 flex items-center justify-center gap-2 min-h-24 w-full hover:preset-filled-surface-100-900 transition cursor-pointer"
          onclick={() => profilesStore.addTier()}
        >
          <Plus class="size-6 opacity-70" />
          <span class="text-sm opacity-70">
            {manage_profiles_add_tier()}
          </span>
        </button>
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

<ConfirmDialog
  open={removeTierIdx !== null}
  title={manage_profiles_remove_tier_confirm_title()}
  description={manage_profiles_remove_tier_confirm_desc()}
  confirmLabel={common_remove()}
  tone="danger"
  onConfirm={onTierRemoveConfirmed}
  onCancel={() => (removeTierIdx = null)}
/>

<ProviderDialog
  open={providerDialog.open}
  mode={providerDialog.mode}
  provider={providerDialog.provider}
  existingIds={providerIds}
  onSave={onProviderSave}
  onCancel={() => (providerDialog.open = false)}
  onRemove={providerDialog.mode === "edit" ? onProviderRemove : null}
/>

<TierDialog
  open={tierDialog.open}
  tierIdx={tierDialog.tierIdx}
  profiles={profilesStore.draft?.tiers[tierDialog.tierIdx]?.profiles ?? []}
  {providerIds}
  onSave={onTierSave}
  onCancel={() => (tierDialog.open = false)}
/>
