<script lang="ts">
  // Profiles manage page — card-based layered view (provider cards in a
  // global pool, named-set containers holding profile cards, an unassigned
  // area for profiles out of rotation), matching the wire shape 1:1.
  // Read-mostly: editing goes through the Provider/Set/Unassigned dialogs;
  // profile cards drag between sets and the unassigned area (HTML5 DnD
  // updating the draft).
  //
  // Probe results, dialog latches and the drag lifecycle live in the page's
  // controllers (profilesProbes, profilesDialogs, profilesDrag): the page
  // keeps the layout, the commit path and the derived posture.
  import { profilesStore } from "../../lib/manage/profiles.svelte.ts";
  import { createProfilesDrag } from "../../lib/manage/profilesDrag.svelte.ts";
  import { createProfilesDialogs } from "../../lib/manage/profilesDialogs.svelte.ts";
  import { createProfilesProbes } from "../../lib/manage/profilesProbes.svelte.ts";
  import { managementBackend } from "../../lib/manage/client.ts";
  import {
    derivedPullKey,
    pullSelectionDirty,
    selectionWire,
    gatewayTabPosture,
  } from "../../lib/manage/profilesGatewayTab.ts";
  import type { PullRow } from "../../lib/manage/profilesGatewayTab.ts";
  import { refreshUnassignedLive as fetchUnassignedLive } from "../../lib/manage/unassignedLive.ts";
  import { adminGatewayPath, gatewayPath } from "../../lib/shell/routes.ts";
  import { shellMode } from "../../lib/shell/port.ts";
  import {
    archeionPolisOriginOrFail,
    archeionSession,
  } from "../../lib/session/archeion.svelte.ts";
  import { userGatewayStore } from "../../lib/gateway/store.svelte.ts";
  import GatewayCollectionsPreview from "../../components/gateway/GatewayCollectionsPreview.svelte";
  import { Tabs } from "@skeletonlabs/skeleton-svelte";
  import ProfilesToolbar from "../../components/manage/ProfilesToolbar.svelte";
  import ProvidersSection from "../../components/manage/ProvidersSection.svelte";
  import SetsSection from "../../components/manage/SetsSection.svelte";
  import UnassignedSection from "../../components/manage/UnassignedSection.svelte";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import ProviderDialog from "../../components/manage/ProviderDialog.svelte";
  import SetDialog from "../../components/manage/SetDialog.svelte";
  import ProfileDialog from "../../components/manage/ProfileDialog.svelte";
  import {
    setDefaultSet,
    replaceUnassignedProfiles,
  } from "../../lib/compute.ts";
  import {
    occupiedIdsOf,
    providerIdsOf,
  } from "../../lib/manage/profiles-view.ts";
  import type { ProfileSet } from "@kallipai/kallipai-client";
  import {
    common_remove,
    common_save,
    manage_profiles_apply,
    manage_profiles_unassigned_dialog_desc,
    manage_profiles_source_proxy_banner,
    manage_profiles_source_local,
    manage_profiles_source_degraded,
    manage_profiles_source_degraded_body,
    manage_profiles_source_healthy,
    manage_profiles_refetch,
    manage_profiles_source_poisoned,
    manage_profiles_source_poisoned_body,
    manage_profiles_open_gateway_admin,
    manage_profiles_sets,
    manage_gateway_health_token_state,
    manage_gateway_health_last_refresh,
    manage_gateway_health_failures,
    manage_profiles_unassigned_dialog_edit_title,
    manage_profiles_unassigned_dialog_new_title,
    manage_profiles_profile_dialog_desc,
    manage_profiles_profile_dialog_edit_title,
    manage_profiles_remove_provider,
    manage_profiles_remove_provider_desc,
    manage_profiles_dangling_desc,
    manage_profiles_dangling_title,
    manage_profiles_apply_desc,
    manage_profiles_apply_desc_unassigned,
    manage_profiles_apply_title,
    manage_profiles_applied_result,
    manage_profiles_remove_set_confirm_desc,
    manage_profiles_remove_set_confirm_title,
    manage_profiles_remove_set_confirm_users_desc,
    manage_profiles_title,
    manage_profiles_tab_gateway,
    manage_profiles_tab_local,
    manage_profiles_switch_entry_title,
    manage_profiles_switch_entry_desc,
    manage_profiles_switch_to_gateway,
    manage_profiles_switch_back_local,
    manage_profiles_local_disk_preview_title,
    manage_profiles_local_disk_summary,
    manage_profiles_local_disk_absent,
    manage_profiles_local_disk_pending,
    manage_profiles_local_disk_unreadable,
    manage_profiles_switch_confirm_title_gateway,
    manage_profiles_switch_confirm_desc_gateway,
    manage_profiles_switch_confirm_title_local,
    manage_profiles_switch_confirm_desc_local,
    manage_profiles_connect_guide_title,
    manage_profiles_guide_link,
    manage_profiles_source_platform,
    manage_profiles_selection_hint,
    manage_profiles_selection_none,
    manage_profiles_rebind_title,
    manage_profiles_rebind_body,
    manage_profiles_unenrolled_title,
    manage_profiles_unenrolled_body,
    manage_profiles_connect_guide_body,
    manage_profiles_connect_guide_offline_body,
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

  // One button, two mode-exclusive pendings: the gateway mirror is
  // read-only (the pick is the only saveable change), the local
  // page has no pick to commit (the form is). The routing keeps
  // each face on its own channel: the pick goes through the
  // same-mode source PUT, the form through the draft save.
  async function onSave() {
    if (selectionDirty) {
      await onApplySelection().catch(() => {});
    } else {
      await profilesStore.save().catch(() => {});
    }
    probes.clearAll();
  }

  async function onConfirmDanglingSave() {
    await profilesStore.save(true).catch(() => {});
    probes.clearAll();
  }

  // The confirm re-send routes by the kind the store recorded on
  // the park, so the retry always matches the live request and a
  // stale closure can never outlive the dialog.
  async function onConfirmDangling() {
    const park = profilesStore.pendingDangling;
    if (park?.retry === "switch") {
      await onSwitchConfirmed(true).catch(() => {});
      return;
    }
    if (park?.retry === "apply") {
      await onApplySelection(true).catch(() => {});
      return;
    }
    await onConfirmDanglingSave();
  }

  // --- inline probe results, routed by call-site scope ---

  const probes = createProfilesProbes({ store: profilesStore });
  const providerReports = probes.providerReports;
  const profileReports = probes.profileReports;

  // Card-level probes read the committed draft; the controller owns the maps.
  const onTestProvider = (id: string) => void probes.cardTestProvider(id);
  const onTestSet = (setName: string) => void probes.cardTestSet(setName);
  const onTestProfile = (setName: string, profileIdx: number) =>
    void probes.cardTestProfile(setName, profileIdx);
  const onTestAll = () => void probes.cardTestAll();
  const onTestUnassigned = (idx: number) => void probes.cardTestUnassigned(idx);

  function onDiscard() {
    profilesStore.reset();
    selection = null;
    probes.clearAll();
    unassignedLive = null;
  }

  // --- unassigned-live warn snapshot (event-driven, advisory) ---

  /** Unassigned ids some live agent still runs, from the last snapshot.
   * Null = no snapshot yet (or nothing unassigned-live); the banner and the
   * apply-confirm extension both render from it. Refreshed when a card
   * enters or leaves the unassigned area, cleared on discard. Never polled:
   * an always-on variant needs the list endpoint to carry the active profile
   * id; that is a backend change, not a frontend poll. */
  let unassignedLive = $state<{
    agentCount: number;
    profileIds: string[];
  } | null>(null);

  // unassignedLive.ts owns the roster fetch and the allSettled fan-out; the
  // wrapper keeps the previous snapshot when the roster itself fails
  // (advisory only).
  async function refreshUnassignedLive(): Promise<void> {
    const res = await fetchUnassignedLive(
      managementBackend(),
      profilesStore.draft?.parking ?? [],
    );
    if (res.refreshed) unassignedLive = res.snapshot;
  }

  // --- drag & drop (profile cards between sets and the unassigned area) ---

  const drag = createProfilesDrag({
    store: profilesStore,
    probes,
    refreshUnassignedLive,
  });

  // --- dialogs ---

  // One controller owns the dialog latches; the page keeps the commit path
  // (apply and the dangling confirm), because those call its own sends.
  const dialogs = createProfilesDialogs({
    store: profilesStore,
    backend: managementBackend,
    probes,
    refreshUnassignedLive,
  });

  function onRemoveSetProfile(
    setName: string,
    profileIdx: number,
    profileId: string,
  ) {
    profilesStore.removeProfile(setName, profileIdx);
    probes.clearSetProfile(setName, profileId);
  }

  function onRemoveUnassigned(idx: number, profileId: string) {
    const draft = profilesStore.draft;
    if (!draft) return;
    profilesStore.draft = replaceUnassignedProfiles(
      draft,
      (draft.parking ?? []).filter((_, i) => i !== idx),
    );
    probes.clearUnassignedProfile(profileId);
    void refreshUnassignedLive();
  }

  const providerIds = $derived(providerIdsOf(profilesStore.draft));

  const setEntries = $derived(
    Object.entries(profilesStore.draft?.sets ?? {}),
  ) as readonly [string, ProfileSet][];
  const setNames = $derived(Object.keys(profilesStore.draft?.sets ?? {}));

  const occupiedIds = $derived(occupiedIdsOf(profilesStore.draft));

  // The profile source decides the page's posture: the model-gateway
  // mode serves a read-only mirror (the gateway admin page owns the
  // mutations), local mode is the full editor. The wire spelling is
  // "model-gateway" everywhere (settings, wire, UI — one word).
  const sourceBlock = $derived(profilesStore.config?.source ?? null);
  const sourceMode = $derived(sourceBlock?.mode ?? "local");
  const sourceHealth = $derived(sourceBlock);
  const proxyAvailable = $derived(sourceBlock?.proxy_available === true);
  const readOnly = $derived(sourceMode === "model-gateway");
  // The source posture: a rejected stored credential poisons the face
  // (the gateway refuses refresh), refresh failures alone degrade it,
  // and local mode keeps the one-line footer.
  const sourceState = $derived(
    sourceMode !== "model-gateway"
      ? "local"
      : sourceHealth?.poisoned === true
        ? "poisoned"
        : (sourceHealth?.refresh_failure_count ?? 0) > 0
          ? "degraded"
          : "healthy",
  );

  // The on-disk local file as the gateway-mode preview: what switching
  // back to local would face. Present only under the model-gateway
  // source (the server omits the key otherwise).
  const localDisk = $derived(profilesStore.config?.local_disk ?? null);
  // Tab shell: the gateway tab needs a platform context — the online
  // shell runs on one platform's app domain, and that platform's
  // enrollment is the switch target (the polis origin the serving
  // archeion exposes). The offline (direct) shell has no platform
  // context, so the switch target is structurally absent there; the
  // tab keeps only the connected mirror and the way back to local.
  // Clicking a tab only looks — every switch goes through the
  // confirm dialog, never the tab itself.
  const online = $derived(shellMode() === "online");
  const sessionReady = $derived(
    online &&
      archeionSession.user !== undefined &&
      archeionSession.user !== null,
  );
  // The serving platform's origin, from the injected archeion base —
  // defined exactly when the online shell booted it.
  const servingPolis = $derived(
    shellMode() === "online" ? archeionPolisOriginOrFail() : null,
  );
  // The posture itself lives in the pure helper (tested there);
  // the page feeds the live inputs and reads the derived flags.
  const posture = $derived(
    gatewayTabPosture({
      sourceMode,
      proxyAvailable,
      online,
      sessionReady,
      servingPolis,
      platforms: sourceBlock?.platforms,
      connectedPolis: sourceHealth?.polis ?? null,
    }),
  );
  const showGatewayTab = $derived(posture.showGatewayTab);
  const servingPlatformEnrolled = $derived(posture.servingPlatformEnrolled);

  // The browse face on the gateway tab: the collection preview reads
  // the caller's own account (the user session), so it renders with a
  // session whether the daemon is connected or not; the pick
  // edits the pull selection whenever the tab is reachable.
  const browseFace = $derived(posture.browseFace);
  let previewLoadDone = $state(false);
  $effect(() => {
    if (browseFace && !previewLoadDone) {
      previewLoadDone = true;
      void userGatewayStore.loadCollections();
    }
  });

  // --- the pull selection (the one-collection pick) ---

  // The browse rows in one list (own collections plus the platform
  // catalog): the baseline and the pick read the same rows,
  // keyed by the shared scheme (see profilesGatewayTab.ts).
  function gatewayRows(): PullRow[] {
    return [
      ...(userGatewayStore.collections ?? []),
      ...(userGatewayStore.platformCatalog ?? []),
    ];
  }

  // The live baseline under the connected source: the pure helper
  // derives it. Null without rows or under the local source (the
  // selected collection is not on the wire); the pick then falls
  // back to nothing, and the empty snapshot reads as no selection.
  const derivedKey = $derived.by(() => {
    if (!readOnly) return null;
    return derivedPullKey(
      gatewayRows(),
      Object.keys(profilesStore.config?.sets ?? {}),
    );
  });

  // The operator's pick, once a row is touched; null = follow the
  // live baseline. Kept after a switch to local: the local config
  // never reflects the selection, so the touched pick stays the
  // display truth until the page reloads.
  let selection = $state<string | null>(null);
  const selectedKey = $derived.by(() => selection ?? derivedKey);
  // Connected to this page's platform: the pick here acts on the
  // live face (the same-mode PUT re-points it).
  const activeHere = $derived(posture.activeHere);
  // The pick dirties only under the connected-here source;
  // while local it rides the switch request instead. An
  // empty live snapshot (no selection yet) is a baseline the
  // pick can depart from, so the first choice is saveable.
  const selectionDirty = $derived(
    pullSelectionDirty(activeHere, selection, derivedKey),
  );
  // The pick renders wherever an action can consume it: the
  // connected-here save, or the pre-switch choice while local on
  // an enrolled platform.
  const selectionEditable = $derived(posture.selectionEditable);

  function pickRow(key: string): void {
    selection = key;
  }

  // Apply the touched pick: a dangling 409 parks the stranded
  // list and the shared confirm dialog re-sends with force. A
  // success resets the touched pick: the rebuilt live face is the
  // baseline again, so no phantom dirty flag survives the save.
  async function onApplySelection(force = false): Promise<void> {
    if (selectedKey === null) return;
    const target = selectionWire(gatewayRows(), selectedKey);
    if (target === null) return;
    try {
      await profilesStore.updateSourceCollection(target, force);
      selection = null;
    } catch {
      // A 409 parks in the store (retry: apply); other failures
      // park the error text on the error line.
    }
  }

  let chosenTab = $state<"gateway" | "local" | null>(null);
  const activeTab = $derived(chosenTab ?? (readOnly ? "gateway" : "local"));
  let showSwitchDialog = $state(false);
  let switchingToGateway = $state(false);
  async function onSwitchConfirmed(force = false): Promise<void> {
    try {
      if (switchingToGateway) {
        // The switch names the serving platform (its origin resolves
        // the entry server-side) and carries a touched pick; an
        // untouched one keeps the selected collection instead of
        // being silently re-pointed.
        const target =
          selection === null ? null : selectionWire(gatewayRows(), selection);
        await profilesStore.switchSource("model-gateway", {
          ...(servingPolis !== null ? { polis: servingPolis } : {}),
          ...(target !== null ? { collection: target } : {}),
          ...(force ? { force: true } : {}),
        });
        // The switch response carries the full source block, so the
        // rebuilt live face is the baseline again; the touched pick
        // resets (the local-bound switch keeps it below: the local
        // config never reflects the selection, and the touched pick
        // stays the display truth until the page reloads).
        selection = null;
      } else {
        await profilesStore.switchSource("local", force ? { force: true } : {});
      }
      showSwitchDialog = false;
    } catch {
      // A 409 parks in the store (retry: switch); other failures
      // park the error text; the dialog stays open for the
      // operator to read it and retry or cancel.
    }
  }
</script>

<svelte:head><title>{manage_profiles_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="px-2 md:p-6 max-w-3xl space-y-6">
    <ProfilesToolbar
      store={profilesStore}
      {applyResult}
      {unassignedLive}
      {onTestAll}
      {onSave}
      {onDiscard}
      onRequestApply={() => (showApplyDialog = true)}
      {readOnly}
      pickDirty={selectionDirty}
    />
    {#if !showGatewayTab}
      <!-- The local-only shape: this daemon carries no enrolled
           platform entry, so the gateway face is structurally absent.
           The guidance names the real knob (relay enrollment on
           the tagmata page); the address follows the platform, so
           nothing to configure for it. -->
      <div
        class="rounded-base border-2 border-surface-500 p-4 text-sm space-y-2"
        role="status"
      >
        <p class="font-medium">{manage_profiles_connect_guide_title()}</p>
        <p class="opacity-70">
          {#if online}{manage_profiles_connect_guide_body()}{:else}
            {manage_profiles_connect_guide_offline_body()}
          {/if}
        </p>
        <!-- Enrollment runs online, so the offline card (whose way
             out is the online shell) omits the link instead of
             pointing at a dead end. -->
        {#if sessionReady && online}
          <a class="underline" href="/tagmata">
            {manage_profiles_guide_link()}
          </a>
        {/if}
      </div>
    {/if}
    <!-- Tab shell: the gateway face (the mirror under the gateway
         source, or the switch entry while local with connection
         params) and the local face (the editor, or the disk preview
         gateway). Tab clicks only look; switches go through the
         confirm dialog. -->
    <Tabs
      value={activeTab}
      onValueChange={(e) => (chosenTab = e.value as "gateway" | "local")}
    >
      {#if showGatewayTab}
        <Tabs.List class="flex gap-2">
          <Tabs.Trigger
            value="gateway"
            class="btn btn-sm {activeTab === 'gateway'
              ? 'preset-filled-primary-500'
              : 'preset-tonal-surface'}"
          >
            {manage_profiles_tab_gateway()}
          </Tabs.Trigger>
          <Tabs.Trigger
            value="local"
            class="btn btn-sm {activeTab === 'local'
              ? 'preset-filled-primary-500'
              : 'preset-tonal-surface'}"
          >
            {manage_profiles_tab_local()}
          </Tabs.Trigger>
        </Tabs.List>
      {/if}

      {#if showGatewayTab}
        <Tabs.Content value="gateway" class="pt-4">
          {#if readOnly}
            <div
              class="rounded-base border-2 p-4 text-sm space-y-1 {sourceState ===
              'poisoned'
                ? 'border-error-500'
                : sourceState === 'degraded'
                  ? 'border-warning-500'
                  : 'border-surface-500'}"
              role="status"
            >
              <div class="flex items-center gap-2">
                {#if sourceState === "poisoned"}
                  <span class="badge preset-filled-error-500 text-xs shrink-0">
                    {manage_profiles_source_poisoned()}
                  </span>
                {:else if sourceState === "degraded"}
                  <span
                    class="badge preset-filled-warning-500 text-xs shrink-0"
                  >
                    {manage_profiles_source_degraded()}
                  </span>
                {:else}
                  <span
                    class="badge preset-filled-surface-500 text-xs shrink-0"
                  >
                    {manage_profiles_source_healthy()}
                  </span>
                {/if}
                <p>{manage_profiles_source_proxy_banner()}</p>
              </div>
              {#if sourceHealth?.polis}
                <p class="opacity-70">
                  {manage_profiles_source_platform({
                    origin: sourceHealth.polis,
                  })}
                </p>
              {/if}
              {#if sourceState === "poisoned"}
                <p class="opacity-70">
                  {manage_profiles_source_poisoned_body()}
                </p>
                {#if sessionReady}
                  <a class="underline" href="/tagmata">
                    {manage_profiles_guide_link()}
                  </a>
                {/if}
              {:else if sourceState === "degraded"}
                <p class="opacity-70">
                  {manage_profiles_source_degraded_body()}
                </p>
              {/if}
              <p class="opacity-70">
                {manage_gateway_health_token_state()}: {sourceHealth?.token_state ??
                  "unknown"}
                · {manage_gateway_health_last_refresh()}: {sourceHealth?.last_refresh ??
                  "unknown"}
                · {manage_gateway_health_failures()}: {sourceHealth?.refresh_failure_count ??
                  0}
              </p>
              <div class="mt-2">
                <button
                  type="button"
                  class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
                  disabled={profilesStore.isRefetching}
                  onclick={() => void profilesStore.refetchSource()}
                >
                  {manage_profiles_refetch()}
                </button>
              </div>
              <!-- The live sets under the gateway source: the config
                   already is the gateway's mirror, so the set names
                   answer directly. -->
              {#if Object.keys(profilesStore.config?.sets ?? {}).length > 0}
                <p class="opacity-70 flex flex-wrap items-center gap-1.5">
                  {manage_profiles_sets()}
                  {#each Object.keys(profilesStore.config?.sets ?? {}) as s (s)}
                    <span
                      class="badge preset-outlined-surface-500 text-xs font-mono"
                    >
                      {s}
                    </span>
                  {/each}
                </p>
              {:else if browseFace}
                <p class="opacity-70">
                  {manage_profiles_selection_none()}
                </p>
              {/if}
              <a class="underline" href={adminGatewayPath()}
                >{manage_profiles_open_gateway_admin()}</a
              >
            </div>
            {#if browseFace}
              <!-- The pull selection under the connected face: the
                   live browse with the pick; a touched pick applies
                   through the same-mode PUT, which re-points the
                   gateway selection and re-fetches the snapshot. -->
              <div class="space-y-3">
                <GatewayCollectionsPreview
                  collections={userGatewayStore.collections ?? []}
                  platformCatalog={userGatewayStore.platformCatalog ?? []}
                  loading={userGatewayStore.isLoading}
                  error={userGatewayStore.error}
                  manageHref={gatewayPath()}
                  {selectedKey}
                  {selectionEditable}
                  onPickRow={pickRow}
                  onRetry={() => void userGatewayStore.loadCollections()}
                />
                <p class="text-xs opacity-60">
                  {manage_profiles_selection_hint()}
                </p>
              </div>
            {/if}
            {#if servingPlatformEnrolled && sourceHealth?.polis !== servingPolis}
              <!-- Connected to another platform while this page's
                   platform is enrolled too: one confirmed PUT
                   rebinds the pull here (the two gateways are
                   mutually exclusive). -->
              <div
                class="rounded-base border-2 border-surface-500 p-4 text-sm space-y-2"
                role="status"
              >
                <p class="font-medium">{manage_profiles_rebind_title()}</p>
                <p class="opacity-70">{manage_profiles_rebind_body()}</p>
                <button
                  type="button"
                  class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
                  onclick={() => {
                    switchingToGateway = true;
                    showSwitchDialog = true;
                  }}
                >
                  {manage_profiles_switch_to_gateway()}
                </button>
              </div>
            {/if}
          {:else if servingPlatformEnrolled}
            <!-- The three pieces in place: the collection browse with
                 the pull pick (the selection rides the switch
                 request), then the switch entry itself. -->
            {#if browseFace}
              <div class="space-y-3">
                <GatewayCollectionsPreview
                  collections={userGatewayStore.collections ?? []}
                  platformCatalog={userGatewayStore.platformCatalog ?? []}
                  loading={userGatewayStore.isLoading}
                  error={userGatewayStore.error}
                  manageHref={gatewayPath()}
                  {selectedKey}
                  selectionEditable
                  onPickRow={pickRow}
                  onRetry={() => void userGatewayStore.loadCollections()}
                />
                <p class="text-xs opacity-60">
                  {manage_profiles_selection_hint()}
                </p>
              </div>
            {/if}
            <!-- Switch entry: local is live and the serving platform
                 is enrolled; one confirmed PUT away from the gateway
                 source. -->
            <div
              class="rounded-base border-2 border-surface-500 p-4 text-sm space-y-2"
              role="status"
            >
              <p class="font-medium">{manage_profiles_switch_entry_title()}</p>
              <p class="opacity-70">{manage_profiles_switch_entry_desc()}</p>
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
                onclick={() => {
                  switchingToGateway = true;
                  showSwitchDialog = true;
                }}
              >
                {manage_profiles_switch_to_gateway()}
              </button>
            </div>
          {:else}
            <!-- Enrolled somewhere else but not on this page's
                 platform: the switch targets the serving platform,
                 so the tab teaches the enrollment instead. -->
            <div
              class="rounded-base border-2 border-surface-500 p-4 text-sm space-y-2"
              role="status"
            >
              <p class="font-medium">{manage_profiles_unenrolled_title()}</p>
              <p class="opacity-70">{manage_profiles_unenrolled_body()}</p>
              {#if sessionReady}
                <a class="underline" href="/tagmata">
                  {manage_profiles_guide_link()}
                </a>
              {/if}
            </div>
          {/if}
        </Tabs.Content>
      {/if}

      <Tabs.Content value="local" class="pt-4">
        {#if readOnly}
          <!-- The on-disk file as a read-only preview of what
               switching back would face. -->
          {#if localDisk && "absent" in localDisk}
            <div
              class="rounded-base border-2 border-surface-500 p-4 text-sm space-y-1"
              role="status"
            >
              <p>{manage_profiles_local_disk_absent()}</p>
              {#if localDisk.error}
                <p class="opacity-70">{localDisk.error}</p>
              {/if}
            </div>
          {:else if localDisk && "unreadable" in localDisk}
            <div
              class="rounded-base border-2 border-surface-500 p-4 text-sm space-y-1"
              role="status"
            >
              <p>
                {manage_profiles_local_disk_unreadable({
                  error: localDisk.unreadable,
                })}
              </p>
            </div>
          {:else if localDisk}
            <div
              class="rounded-base border-2 border-surface-500 p-4 text-sm space-y-2"
              role="status"
            >
              <p class="font-medium">
                {manage_profiles_local_disk_preview_title()}
              </p>
              <p class="opacity-70">
                {manage_profiles_local_disk_summary({
                  sets: Object.keys(localDisk.sets ?? {}).length,
                  providers: Object.keys(localDisk.endpoints ?? {}).length,
                  unassigned: (localDisk.parking ?? []).length,
                })}
              </p>
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-primary-500"
                onclick={() => {
                  switchingToGateway = false;
                  showSwitchDialog = true;
                }}
              >
                {manage_profiles_switch_back_local()}
              </button>
            </div>
          {:else}
            <p class="text-xs opacity-60">
              {manage_profiles_local_disk_pending()}
            </p>
          {/if}
        {:else}
          <p class="text-xs opacity-60">{manage_profiles_source_local()}</p>

          <!-- Providers: global pool of provider cards -->
          {#if profilesStore.draft}
            <ProvidersSection
              providers={Object.values(profilesStore.draft.endpoints)}
              reports={providerReports}
              isProbing={profilesStore.isProbing}
              onTest={onTestProvider}
              onEdit={(provider) => dialogs.provider.show(provider)}
              onAdd={() => dialogs.provider.create()}
              onRemove={(provider) => dialogs.providerRemove.ask(provider)}
              {readOnly}
            />

            <SetsSection
              sets={setEntries}
              defaultName={profilesStore.draft.default}
              reports={profileReports}
              isProbing={profilesStore.isProbing}
              dragOverSet={drag.overSet}
              onCardDragStart={(fromSet, fromIdx) =>
                drag.start({ area: "set", fromSet, fromIdx })}
              onCardDragEnd={() => drag.clear()}
              onSetDragOver={(setName) => drag.enterSet(setName)}
              onSetDragLeave={(setName) => drag.leaveSet(setName)}
              onSetDrop={(toSet) => drag.dropToSet(toSet)}
              {onTestSet}
              {onTestProfile}
              onEditSet={(setName) => dialogs.set.show(setName)}
              onEditProfile={(setName, idx) =>
                dialogs.profile.member.show(setName, idx)}
              onRemoveSet={(setName) => void dialogs.setRemove.ask(setName)}
              onSetDefault={(setName) => {
                const draft = profilesStore.draft;
                if (draft) profilesStore.draft = setDefaultSet(draft, setName);
              }}
              onAddSet={() => profilesStore.addSet()}
              onRemoveProfile={onRemoveSetProfile}
              {readOnly}
            />

            <UnassignedSection
              unassigned={profilesStore.draft.parking ?? []}
              reports={profileReports}
              isProbing={profilesStore.isProbing}
              dragOverUnassigned={drag.overUnassigned}
              onCardDragStart={(fromIdx) =>
                drag.start({ area: "unassigned", fromSet: null, fromIdx })}
              onCardDragEnd={() => drag.clear()}
              onUnassignedDragOver={() => drag.enterUnassigned()}
              onUnassignedDragLeave={() => drag.leaveUnassigned()}
              onUnassignedDrop={() => drag.dropToUnassigned()}
              onTest={onTestUnassigned}
              onEdit={(idx) => dialogs.profile.unassigned.show(idx)}
              onAdd={() => dialogs.profile.unassigned.create()}
              onRemove={onRemoveUnassigned}
              {readOnly}
            />
          {/if}
        {/if}
      </Tabs.Content>
    </Tabs>
  </div>
</div>

<ConfirmDialog
  busy={profilesStore.isSaving}
  open={showApplyDialog}
  title={manage_profiles_apply_title()}
  description={unassignedLive
    ? `${manage_profiles_apply_desc()} ${manage_profiles_apply_desc_unassigned({
        count: unassignedLive.agentCount,
      })}`
    : manage_profiles_apply_desc()}
  confirmLabel={manage_profiles_apply()}
  tone="primary"
  onConfirm={onApply}
  onCancel={() => (showApplyDialog = false)}
/>

<ConfirmDialog
  open={dialogs.setRemove.name !== null}
  title={manage_profiles_remove_set_confirm_title()}
  description={dialogs.setRemove.error
    ? dialogs.setRemove.error
    : dialogs.setRemove.users.length > 0
      ? manage_profiles_remove_set_confirm_users_desc({
          users: dialogs.setRemove.users.join(", "),
        })
      : manage_profiles_remove_set_confirm_desc()}
  confirmLabel={common_remove()}
  tone="danger"
  onConfirm={() => void dialogs.setRemove.confirm()}
  onCancel={() => dialogs.setRemove.cancel()}
/>

<ProviderDialog
  open={dialogs.provider.open}
  mode={dialogs.provider.mode}
  provider={dialogs.provider.provider}
  existingIds={providerIds}
  onSave={(result) => dialogs.provider.save(result)}
  onCancel={() => dialogs.provider.close()}
/>

<SetDialog
  open={dialogs.set.open}
  name={dialogs.set.name}
  description={profilesStore.draft?.sets[dialogs.set.name]?.description ?? null}
  allNames={setNames}
  profiles={profilesStore.draft?.sets[dialogs.set.name]?.profiles ?? []}
  {providerIds}
  onSave={(result) => dialogs.set.save(result)}
  onCancel={() => dialogs.set.close()}
/>

<ProfileDialog
  open={dialogs.profile.unassigned.open}
  mode={dialogs.profile.unassigned.mode}
  description={manage_profiles_unassigned_dialog_desc()}
  title={dialogs.profile.unassigned.mode === "new"
    ? manage_profiles_unassigned_dialog_new_title()
    : manage_profiles_unassigned_dialog_edit_title()}
  profile={profilesStore.draft?.parking?.[dialogs.profile.unassigned.idx] ??
    null}
  {providerIds}
  {occupiedIds}
  probeReport={dialogs.profile.unassigned.report}
  probeError={profilesStore.probeError}
  onSave={(values) => dialogs.profile.unassigned.save(values)}
  onCancel={() => dialogs.profile.unassigned.close()}
  onTest={(values) => void dialogs.profile.unassigned.test(values)}
  onRemove={dialogs.profile.unassigned.mode === "edit"
    ? () => dialogs.profile.unassigned.remove()
    : null}
/>
<ProfileDialog
  open={dialogs.profile.member.open}
  mode="edit"
  title={manage_profiles_profile_dialog_edit_title()}
  description={manage_profiles_profile_dialog_desc()}
  profile={profilesStore.draft?.sets[dialogs.profile.member.setName]?.profiles[
    dialogs.profile.member.idx
  ] ?? null}
  {providerIds}
  {occupiedIds}
  probeReport={dialogs.profile.member.report}
  probeError={profilesStore.probeError}
  onSave={(values) => dialogs.profile.member.save(values)}
  onCancel={() => dialogs.profile.member.close()}
  onTest={(values) => void dialogs.profile.member.test(values)}
  onRemove={null}
/>

<ConfirmDialog
  open={profilesStore.pendingDangling !== null}
  busy={profilesStore.isSaving || profilesStore.saveBlocked}
  error={profilesStore.saveBlocked ? profilesStore.error : null}
  title={manage_profiles_dangling_title()}
  description={manage_profiles_dangling_desc({
    count: profilesStore.pendingDangling?.names.length ?? 0,
    list: profilesStore.pendingDangling?.names.join("\n") ?? "",
  })}
  confirmLabel={common_save()}
  tone="primary"
  onConfirm={onConfirmDangling}
  onCancel={() => profilesStore.dismissDangling()}
/>

<ConfirmDialog
  open={dialogs.providerRemove.target !== null}
  title={manage_profiles_remove_provider()}
  description={manage_profiles_remove_provider_desc()}
  confirmLabel={common_remove()}
  tone="danger"
  onConfirm={() => dialogs.providerRemove.confirm()}
  onCancel={() => dialogs.providerRemove.cancel()}
/>

<ConfirmDialog
  busy={profilesStore.isSaving}
  open={showSwitchDialog}
  title={switchingToGateway
    ? manage_profiles_switch_confirm_title_gateway()
    : manage_profiles_switch_confirm_title_local()}
  description={switchingToGateway
    ? manage_profiles_switch_confirm_desc_gateway()
    : manage_profiles_switch_confirm_desc_local()}
  confirmLabel={switchingToGateway
    ? manage_profiles_switch_to_gateway()
    : manage_profiles_switch_back_local()}
  tone="primary"
  error={profilesStore.error}
  onConfirm={onSwitchConfirmed}
  onCancel={() => (showSwitchDialog = false)}
/>
