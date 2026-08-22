<script lang="ts">
  // Profiles manage page — card-based layered view (provider cards in a
  // global pool, tier containers holding profile cards, a parking area for
  // profiles out of rotation), matching the wire shape 1:1. Read-mostly:
  // editing goes through the Provider/Tier/Parking dialogs; profile cards
  // drag between tiers and parking (HTML5 DnD updating the draft).
  //
  // Probe results route inline to the card that triggered them: the page
  // accumulates providerReports/profileReports maps keyed by id, because the
  // store's single `probe` field is replaced wholesale on every call.
  import { profilesStore } from "../../lib/manage/profiles.svelte.ts";
  import { managementBackend } from "../../lib/manage/client.ts";
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
  import ParkingDialog from "../../components/manage/ParkingDialog.svelte";
  import {
    moveProfile,
    moveFromParking,
    moveToParking,
    replaceTierProfiles,
    replaceParkingProfiles,
    singleProfileProbeRequest,
    singleParkingProfileProbeRequest,
    upsertProvider,
  } from "../../lib/manage/compute.ts";
  import { TONAL_ICON_SURF } from "../../lib/classes.ts";
  import {
    clearProfileResult,
    mergeProfileScope,
    mergeProfileScopeAll,
    mergeProviderScope,
    modelsCountLabel,
    occupiedIdsOf,
    parkedLiveSnapshot,
    profileKey,
    probeStatusColor,
    probeStatusLabel,
    providerIdsOf,
  } from "../../lib/manage/profiles-view.ts";
  import type {
    ProfileProvider,
    ProfileProviderProbeReport,
    ProfileModelProbeReport,
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
    manage_profiles_apply_desc_parked,
    manage_profiles_parking_warn,
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
    manage_profiles_parking,
    manage_profiles_parking_add,
    manage_profiles_parking_desc_l1,
    manage_profiles_parking_desc_l2,
    manage_profiles_probe_request_failed,
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

  async function onTestProvider(id: string) {
    await profilesStore.probeProvider(id);
    if (profilesStore.probe) {
      mergeProviderScope(providerReports, profilesStore.probe);
    }
  }

  async function onTestTier(tierIdx: number) {
    await profilesStore.probeTier(tierIdx);
    if (profilesStore.probe) {
      mergeProfileScope(tierIdx, profileReports, profilesStore.probe);
    }
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
    mergeProviderScope(providerReports, resp);
    mergeProfileScope(tierIdx, profileReports, resp);
  }

  async function onTestAll() {
    await profilesStore.probeAll();
    if (!profilesStore.probe) return;
    mergeProviderScope(providerReports, profilesStore.probe);
    mergeProfileScopeAll(profileReports, profilesStore.probe);
  }

  function onDiscard() {
    profilesStore.reset();
    providerReports.clear();
    profileReports.clear();
    parkedLive = null;
  }

  // --- drag & drop (profile cards between tiers and the parking area) ---

  interface DragPayload {
    area: "tier" | "parking";
    fromTier: number;
    fromIdx: number;
  }

  let drag = $state<DragPayload | null>(null);
  let dragOverTier = $state(-1);
  let dragOverParking = $state(false);

  function onDropTier(toTier: number): void {
    const d = drag;
    drag = null;
    dragOverTier = -1;
    dragOverParking = false;
    const draft = profilesStore.draft;
    if (!d || !draft) return;
    if (d.area === "parking") {
      // parking → tier: the p:-keyed report is area-scoped, clear it.
      const id = draft.parking?.[d.fromIdx]?.id;
      if (id) profileReports.delete(`p:${id}`);
      profilesStore.draft = moveFromParking(draft, d.fromIdx, toTier);
      void refreshParkedLive();
      return;
    }
    if (d.fromTier !== toTier) {
      // Cross-tier: the key is tier-scoped, so clear the stale source entry
      // (same-tier keeps its key — the report survives the reorder).
      const id = draft.tiers[d.fromTier]?.profiles[d.fromIdx]?.id;
      if (id) clearProfileResult(profileReports, d.fromTier, id);
    }
    profilesStore.draft = moveProfile(draft, d.fromTier, d.fromIdx, toTier);
  }

  function onDropParking(): void {
    const d = drag;
    drag = null;
    dragOverTier = -1;
    dragOverParking = false;
    const draft = profilesStore.draft;
    if (!d || !draft || d.area !== "tier") return;
    // tier → parking: clear the tier-scoped source entry; the card will
    // re-key its report as p:<id> on the next parking Test.
    const id = draft.tiers[d.fromTier]?.profiles[d.fromIdx]?.id;
    if (id) clearProfileResult(profileReports, d.fromTier, id);
    profilesStore.draft = moveToParking(draft, d.fromTier, d.fromIdx);
    void refreshParkedLive();
  }

  // --- parked-live warn snapshot (event-driven, advisory) ---

  /** Parked ids some live agent still runs, from the last snapshot.
   * Null = no snapshot yet (or nothing parked-live); the banner and the
   * apply-confirm extension both render from it. Refreshed by parking/
   * unparking mutations, cleared on discard — never polled (a later
   * always-on variant needs the list endpoint to carry the active profile
   * id; that is a backend change, not a frontend poll). */
  let parkedLive = $state<{ agentCount: number; profileIds: string[] } | null>(
    null,
  );

  async function refreshParkedLive(): Promise<void> {
    const draft = profilesStore.draft;
    const parked = draft?.parking ?? [];
    if (parked.length === 0) {
      parkedLive = null;
      return;
    }
    const parkedIds = parked.map((p) => p.id);
    try {
      const { agents } = await managementBackend().listAgents();
      // Per-agent catch (allSettled): one 409/404 must not void the whole
      // snapshot — the advisory layer reports what it could see.
      const statuses = await Promise.allSettled(
        agents.map((a) => managementBackend().getAgentStatus(a.id)),
      );
      parkedLive = parkedLiveSnapshot(parkedIds, statuses);
    } catch {
      // Roster failure: leave the previous snapshot (advisory only).
    }
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

  // Parking dialog: single-profile form (see ParkingDialog). idx indexes the
  // draft's parked list in edit mode.
  let parkingDialog = $state<{
    open: boolean;
    mode: "new" | "edit";
    idx: number;
  }>({ open: false, mode: "new", idx: 0 });
  // Latest in-form probe result, rendered inside the dialog.
  let parkingProbeReport = $state<{
    status: string;
    detail: string | null;
  } | null>(null);

  function openParkingNew() {
    parkingProbeReport = null;
    parkingDialog = { open: true, mode: "new", idx: 0 };
  }

  function openParkingEdit(idx: number) {
    parkingProbeReport = null;
    parkingDialog = { open: true, mode: "edit", idx };
  }

  function onParkingSave(values: {
    id: string;
    endpoint: string;
    model: string;
    max_context_window: number;
  }) {
    const draft = profilesStore.draft;
    if (!draft) return;
    const list = [...(draft.parking ?? [])];
    if (parkingDialog.mode === "new") list.push(values);
    else list[parkingDialog.idx] = values;
    profilesStore.draft = replaceParkingProfiles(draft, list);
    parkingDialog.open = false;
    void refreshParkedLive();
  }

  function onParkingRemove() {
    const draft = profilesStore.draft;
    if (draft && parkingDialog.mode === "edit") {
      const id = draft.parking?.[parkingDialog.idx]?.id;
      profilesStore.draft = replaceParkingProfiles(
        draft,
        (draft.parking ?? []).filter((_, i) => i !== parkingDialog.idx),
      );
      if (id) profileReports.delete(`p:${id}`);
    }
    void refreshParkedLive();
    parkingDialog.open = false;
  }

  /** Probe the dialog's current form values without touching the draft:
   * stage them into a throwaway copy and reuse the parked-profile request
   * builder (committed config passed for the masked-key rule). */
  async function onParkingTest(values: {
    id: string;
    endpoint: string;
    model: string;
    max_context_window: number;
  }) {
    const draft = profilesStore.draft;
    if (!draft) return;
    const staged = replaceParkingProfiles(draft, [
      ...(draft.parking ?? []),
      values,
    ]);
    const body = singleParkingProfileProbeRequest(
      profilesStore.config,
      staged,
      (staged.parking?.length ?? 1) - 1,
    );
    if (!body) return;
    const resp = await profilesStore.probeRaw(body);
    if (!resp) return;
    mergeProviderScope(providerReports, resp);
    const p = resp.tiers[0]?.profiles[0];
    if (p) parkingProbeReport = { status: p.status, detail: p.detail ?? null };
  }

  /** Kebab Test on a parked card: same request shape, from the draft. */
  async function onTestParking(idx: number) {
    const draft = profilesStore.draft;
    if (!draft) return;
    const body = singleParkingProfileProbeRequest(
      profilesStore.config,
      draft,
      idx,
    );
    if (!body) return;
    const resp = await profilesStore.probeRaw(body);
    if (!resp) return;
    mergeProviderScope(providerReports, resp);
    const p = resp.tiers[0]?.profiles[0];
    if (p) {
      const id = draft.parking?.[idx]?.id ?? p.profile_id;
      profileReports.set(`p:${id}`, p);
    }
  }

  const providerIds = $derived(providerIdsOf(profilesStore.draft));

  const occupiedIds = $derived(occupiedIdsOf(profilesStore.draft));
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
      <div class="flex flex-wrap gap-2">
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

      <!-- Providers: global pool of provider cards -->
      <section class="space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_profiles_providers()}
        </h2>
        <div class="grid gap-3 sm:grid-cols-2">
          {#each Object.values(profilesStore.draft.endpoints) as ep (ep.id)}
            {@const report = providerReports.get(ep.id)}
            <div class="card preset-tonal-surface p-4 space-y-2 min-w-0">
              <div class="flex items-center justify-between gap-2">
                <span
                  class="font-mono text-sm font-semibold truncate min-w-0 flex-1"
                  >{ep.id}</span
                >
                <Menu
                  positioning={{ placement: "bottom-end" }}
                  onSelect={(e) => {
                    if (e.value === "test") onTestProvider(ep.id);
                    else if (e.value === "edit") openProviderEdit(ep);
                  }}
                >
                  <Menu.Trigger
                    class="size-10 {TONAL_ICON_SURF} shrink-0"
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
                  <dt class="opacity-60 shrink-0">
                    {manage_profiles_profile_provider_label()}:
                  </dt>
                  <dd class="font-mono min-w-0">{ep.family}</dd>
                </div>
                <div class="flex gap-2">
                  <dt class="opacity-60 shrink-0">
                    {manage_profiles_provider_card_base_url_label()}:
                  </dt>
                  <dd class="font-mono truncate min-w-0">
                    {ep.base_url ?? manage_profiles_provider_base_url_default()}
                  </dd>
                </div>
                <div class="flex gap-2">
                  <dt class="opacity-60 shrink-0">API key:</dt>
                  <dd class="font-mono min-w-0 break-all">{ep.api_key}</dd>
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
              onDropTier(tierIdx);
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
                  class="size-10 {TONAL_ICON_SURF} shrink-0"
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
                  drag = {
                    area: "tier",
                    fromTier: tierIdx,
                    fromIdx: profileIdx,
                  };
                  // Firefox only starts a drag session if dataTransfer gets data.
                  if (e.dataTransfer) {
                    e.dataTransfer.setData("text/plain", profile.id);
                    e.dataTransfer.effectAllowed = "move";
                  }
                }}
                ondragend={() => {
                  drag = null;
                  dragOverTier = -1;
                  dragOverParking = false;
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
                      class="size-10 {TONAL_ICON_SURF} shrink-0"
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

      <!-- Parking: profiles held out of rotation. Same card language as the
           tiers above, but the container stays dashed (not a rotation slot)
           and its add button opens the single-profile ParkingDialog. -->
      <section class="space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {manage_profiles_parking()}
        </h2>
        <div class="text-xs opacity-60 mt-1 space-y-0.5">
          <p>{manage_profiles_parking_desc_l1()}</p>
          <p>{manage_profiles_parking_desc_l2()}</p>
        </div>
        <div
          role="list"
          class="card preset-tonal-surface border-2 border-dashed border-surface-400 p-4 space-y-3 {dragOverParking
            ? 'outline-2 outline-dashed outline-primary-500'
            : ''}"
          ondragover={(e) => {
            e.preventDefault();
            dragOverParking = true;
          }}
          ondragleave={() => (dragOverParking = false)}
          ondrop={(e) => {
            e.preventDefault();
            onDropParking();
          }}
        >
          {#each profilesStore.draft.parking ?? [] as profile, idx (idx)}
            {@const report = profileReports.get(`p:${profile.id}`)}
            <div
              role="listitem"
              class="card preset-filled-surface-100-900 p-3 space-y-1 cursor-grab"
              draggable="true"
              ondragstart={(e) => {
                drag = { area: "parking", fromTier: -1, fromIdx: idx };
                if (e.dataTransfer) {
                  e.dataTransfer.setData("text/plain", profile.id);
                  e.dataTransfer.effectAllowed = "move";
                }
              }}
              ondragend={() => {
                drag = null;
                dragOverTier = -1;
                dragOverParking = false;
              }}
            >
              <div class="flex items-center justify-between gap-2">
                <span class="font-mono text-sm">{profile.id}</span>
                <Menu
                  positioning={{ placement: "bottom-end" }}
                  onSelect={(e) => {
                    if (e.value === "test") onTestParking(idx);
                    else if (e.value === "edit") openParkingEdit(idx);
                  }}
                >
                  <Menu.Trigger
                    class="size-10 {TONAL_ICON_SURF} shrink-0"
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
          <button
            type="button"
            class="card preset-tonal-surface border-2 border-dashed border-surface-400 p-4 flex items-center justify-center gap-2 min-h-24 w-full hover:preset-filled-surface-100-900 transition cursor-pointer"
            onclick={openParkingNew}
          >
            <Plus class="size-6 opacity-70" />
            <span class="text-sm opacity-70">
              {manage_profiles_parking_add()}
            </span>
          </button>
        </div>
      </section>
    {/if}
  </div>
</div>

<ConfirmDialog
  busy={profilesStore.isSaving}
  open={showApplyDialog}
  title={manage_profiles_apply_title()}
  description={parkedLive
    ? `${manage_profiles_apply_desc()} ${manage_profiles_apply_desc_parked({
        count: parkedLive.agentCount,
      })}`
    : manage_profiles_apply_desc()}
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

<ParkingDialog
  open={parkingDialog.open}
  mode={parkingDialog.mode}
  profile={profilesStore.draft?.parking?.[parkingDialog.idx] ?? null}
  {providerIds}
  {occupiedIds}
  probeReport={parkingProbeReport}
  onSave={onParkingSave}
  onCancel={() => (parkingDialog.open = false)}
  onTest={onParkingTest}
  onRemove={parkingDialog.mode === "edit" ? onParkingRemove : null}
/>
