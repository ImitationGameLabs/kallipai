<script lang="ts">
  // Profiles manage page — card-based layered view (provider cards in a
  // global pool, named-set containers holding profile cards, a parking
  // area for profiles out of rotation), matching the wire shape 1:1.
  // Read-mostly: editing goes through the Provider/Set/Parking dialogs;
  // profile cards drag between sets and parking (HTML5 DnD updating the
  // draft).
  //
  // Probe results route inline to the card that triggered them: the page
  // accumulates providerReports/profileReports maps keyed by id, because the
  // store's single `probe` field is replaced wholesale on every call.
  import { profilesStore } from "../../lib/manage/profiles.svelte.ts";
  import { managementBackend } from "../../lib/manage/client.ts";
  import {
    derivedPullKey,
    pullSelectionDirty,
    selectionWire,
    gatewayTabPosture,
  } from "../../lib/manage/profilesGatewayTab.ts";
  import type { PullRow } from "../../lib/manage/profilesGatewayTab.ts";
  import { refreshParkedLive as fetchParkedLive } from "../../lib/manage/parkedLive.ts";
  import { displayError } from "../../lib/manage/errors.ts";
  import { KallipaiError } from "@kallipai/kallipai-common";
  import { adminGatewayPath, gatewayPath } from "../../lib/shell/routes.ts";
  import { shellMode } from "../../lib/shell/port.ts";
  import {
    archeionPolisOriginOrFail,
    archeionSession,
  } from "../../lib/session/archeion.svelte.ts";
  import { userGatewayStore } from "../../lib/gateway/store.svelte.ts";
  import GatewayCollectionsPreview from "../../components/gateway/GatewayCollectionsPreview.svelte";
  import { SvelteMap } from "svelte/reactivity";
  import { Tabs } from "@skeletonlabs/skeleton-svelte";
  import ProfilesToolbar from "../../components/manage/ProfilesToolbar.svelte";
  import ProvidersSection from "../../components/manage/ProvidersSection.svelte";
  import SetsSection from "../../components/manage/SetsSection.svelte";
  import ParkingSection from "../../components/manage/ParkingSection.svelte";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import ProviderDialog from "../../components/manage/ProviderDialog.svelte";
  import SetDialog from "../../components/manage/SetDialog.svelte";
  import ProfileDialog from "../../components/manage/ProfileDialog.svelte";
  import {
    moveProfile,
    moveFromParking,
    moveToParking,
    replaceSetProfiles,
    replaceSetProfile,
    renameSet,
    setDefaultSet,
    updateSetDescription,
    replaceParkingProfiles,
    singleProfileProbeRequest,
    singleParkingProfileProbeRequest,
    upsertProvider,
  } from "../../lib/manage/compute.ts";
  import {
    clearProfileResult,
    mergeProfileScope,
    mergeProfileScopeAll,
    mergeProviderScope,
    occupiedIdsOf,
    providerIdsOf,
  } from "../../lib/manage/profiles-view.ts";
  import type {
    ProfileProvider,
    Modality,
    ProfileProviderProbeReport,
    ProfileModelProbeReport,
    ProfileSet,
    ReasoningEffort,
  } from "@kallipai/kallipai-client";
  import {
    common_remove,
    common_save,
    manage_profiles_apply,
    manage_profiles_parking_dialog_desc,
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
    manage_profiles_parking_dialog_edit_title,
    manage_profiles_parking_dialog_new_title,
    manage_profiles_profile_dialog_desc,
    manage_profiles_profile_dialog_edit_title,
    manage_profiles_remove_provider,
    manage_profiles_remove_provider_desc,
    manage_profiles_dangling_desc,
    manage_profiles_dangling_title,
    manage_profiles_apply_desc,
    manage_profiles_apply_desc_parked,
    manage_profiles_apply_title,
    manage_profiles_applied_result,
    manage_profiles_remove_set_confirm_desc,
    manage_profiles_remove_set_confirm_title,
    manage_profiles_remove_set_confirm_users_desc,
    manage_profiles_remove_set_failed,
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
    providerReports.clear();
    profileReports.clear();
  }

  async function onConfirmDanglingSave() {
    await profilesStore.save(true).catch(() => {});
    providerReports.clear();
    profileReports.clear();
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

  const providerReports = new SvelteMap<string, ProfileProviderProbeReport>();
  const profileReports = new SvelteMap<string, ProfileModelProbeReport>();

  async function onTestProvider(id: string) {
    await profilesStore.probeProvider(id);
    if (profilesStore.probe) {
      mergeProviderScope(providerReports, profilesStore.probe);
    }
  }

  async function onTestSet(setName: string) {
    await profilesStore.probeSet(setName);
    if (profilesStore.probe) {
      mergeProfileScope(setName, profileReports, profilesStore.probe);
    }
  }

  async function onTestProfile(setName: string, profileIdx: number) {
    const draft = profilesStore.draft;
    if (!draft) return;
    const body = singleProfileProbeRequest(
      profilesStore.config,
      draft,
      setName,
      profileIdx,
    );
    if (!body) return;
    const resp = await profilesStore.probeRaw(body);
    if (!resp) return;
    mergeProviderScope(providerReports, resp);
    mergeProfileScope(setName, profileReports, resp);
  }

  async function onTestAll() {
    await profilesStore.probeAll();
    if (!profilesStore.probe) return;
    mergeProviderScope(providerReports, profilesStore.probe);
    mergeProfileScopeAll(profileReports, profilesStore.probe);
  }

  function onDiscard() {
    profilesStore.reset();
    selection = null;
    providerReports.clear();
    profileReports.clear();
    parkedLive = null;
  }

  // --- drag & drop (profile cards between sets and the parking area) ---

  interface DragPayload {
    area: "set" | "parking";
    fromSet: string | null;
    fromIdx: number;
  }

  let drag = $state<DragPayload | null>(null);
  let dragOverSet = $state<string | null>(null);
  let dragOverParking = $state(false);

  // Shared drag-end reset for both drop targets (card sections own the
  // markup, the page owns the drag state).
  function clearDrag(): void {
    drag = null;
    dragOverSet = null;
    dragOverParking = false;
  }

  function onDropSet(toSet: string): void {
    const d = drag;
    drag = null;
    dragOverSet = null;
    dragOverParking = false;
    const draft = profilesStore.draft;
    if (!d || !draft) return;
    if (d.area === "parking") {
      // parking → set: the p:-keyed report is area-scoped, clear it.
      const id = draft.parking?.[d.fromIdx]?.id;
      if (id) profileReports.delete(`p:${id}`);
      profilesStore.draft = moveFromParking(draft, d.fromIdx, toSet);
      void refreshParkedLive();
      return;
    }
    if (d.fromSet !== null && d.fromSet !== toSet) {
      // Cross-set: the key is set-scoped, so clear the stale source entry
      // (same-set keeps its key — the report survives the reorder).
      const id = draft.sets[d.fromSet]?.profiles[d.fromIdx]?.id;
      if (id) clearProfileResult(profileReports, d.fromSet, id);
    }
    if (d.fromSet !== null) {
      profilesStore.draft = moveProfile(draft, d.fromSet, d.fromIdx, toSet);
    }
  }

  function onDropParking(): void {
    const d = drag;
    drag = null;
    dragOverSet = null;
    dragOverParking = false;
    const draft = profilesStore.draft;
    if (!d || !draft || d.area !== "set" || d.fromSet === null) return;
    // set → parking: clear the set-scoped source entry; the card will
    // re-key its report as p:<id> on the next parking Test.
    const id = draft.sets[d.fromSet]?.profiles[d.fromIdx]?.id;
    if (id) clearProfileResult(profileReports, d.fromSet, id);
    profilesStore.draft = moveToParking(draft, d.fromSet, d.fromIdx);
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

  // parkedLive.ts owns the roster fetch and the allSettled fan-out; the
  // wrapper keeps the previous snapshot when the roster itself fails
  // (advisory only).
  async function refreshParkedLive(): Promise<void> {
    const res = await fetchParkedLive(
      managementBackend(),
      profilesStore.draft?.parking ?? [],
    );
    if (res.refreshed) parkedLive = res.snapshot;
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

  let removeProviderTarget = $state<ProfileProvider | null>(null);

  function onProviderRemove(provider: ProfileProvider) {
    // The menu only arms the confirm; the removal itself happens on
    // confirm (draft-level, reversible by discarding the draft).
    removeProviderTarget = provider;
  }

  function onProviderRemoveConfirmed() {
    if (removeProviderTarget === null) return;
    profilesStore.removeProvider(removeProviderTarget.id);
    providerReports.delete(removeProviderTarget.id);
    removeProviderTarget = null;
  }

  // Set removal confirm: a set with bound users cannot be dropped by the
  // draft alone. Opening the confirm lists the users (from the live
  // registry); confirming deletes the set server-side with force (the
  // users are interrupted and keep a dangling binding until rebound). A
  // set with no users stays a pure draft edit.
  let removeSetName = $state<string | null>(null);
  let removeSetUsers = $state<string[]>([]);
  let removeSetError = $state<string | null>(null);

  async function onSetRemoveRequested(name: string) {
    removeSetName = name;
    removeSetUsers = [];
    removeSetError = null;
    try {
      const { agents } = await managementBackend().listAgents();
      removeSetUsers = agents
        .filter((a) => a.profile_set === name)
        .map((a) => a.role || a.id);
    } catch {
      // The user list is advisory; the delete itself surfaces failures.
    }
  }

  async function onSetRemoveConfirmed() {
    const name = removeSetName;
    if (name === null) return;
    removeSetError = null;
    if (removeSetUsers.length > 0) {
      try {
        await managementBackend().deleteProfileSet(name, true);
      } catch (e) {
        // 404 = the set is already gone server-side (dangling users);
        // the local removal is still right. Everything else surfaces.
        if (!(e instanceof KallipaiError && e.api.status === 404)) {
          removeSetError = displayError(
            "profiles",
            e,
            manage_profiles_remove_set_failed(),
          );
          return;
        }
      }
    }
    profilesStore.removeSet(name);
    profileReports.clear();
    removeSetName = null;
    removeSetUsers = [];
  }
  let setDialog = $state<{ open: boolean; name: string }>({
    open: false,
    name: "",
  });

  function onSetSave(result: {
    name: string;
    description: string | null;
    rows: {
      id: string;
      endpoint: string;
      model: string;
      max_context_window: number;
      store?: boolean;
      effort?: ReasoningEffort;
    }[];
  }) {
    const draft = profilesStore.draft;
    if (!draft) return;
    // Rename first (it re-keys), then description, then the profile list
    // — each helper addresses the set by its name at that step.
    const renamed = renameSet(draft, setDialog.name, result.name);
    const described = updateSetDescription(
      renamed,
      result.name,
      result.description,
    );
    profilesStore.draft = replaceSetProfiles(
      described,
      result.name,
      result.rows,
    );
    if (result.name !== setDialog.name) profileReports.clear();
    setDialog.open = false;
  }

  // Set-member profile dialog: single-profile form (see ProfileDialog),
  // positioned at (setName, idx) in the draft's set.
  let profileDialog = $state<{
    open: boolean;
    setName: string;
    idx: number;
  }>({ open: false, setName: "", idx: 0 });
  let profileProbeReport = $state<{
    status: string;
    detail: string | null;
  } | null>(null);

  function openProfileEdit(setName: string, idx: number) {
    profileProbeReport = null;
    profileDialog = { open: true, setName, idx };
  }

  function onProfileSave(values: {
    id: string;
    endpoint: string;
    model: string;
    max_context_window: number;
    store?: boolean;
    effort?: ReasoningEffort;
    modalities?: readonly Modality[];
  }) {
    const draft = profilesStore.draft;
    if (!draft) return;
    profilesStore.draft = replaceSetProfile(
      draft,
      profileDialog.setName,
      profileDialog.idx,
      values,
    );
    profileDialog.open = false;
  }

  /** Probe the dialog's current form values without touching the draft:
   * stage them into a throwaway copy and reuse the set-profile request
   * builder (committed config passed for the masked-key rule). */
  async function onProfileTest(values: {
    id: string;
    endpoint: string;
    model: string;
    max_context_window: number;
    store?: boolean;
    effort?: ReasoningEffort;
    modalities?: readonly Modality[];
  }) {
    const draft = profilesStore.draft;
    if (!draft) return;
    const staged = replaceSetProfile(
      draft,
      profileDialog.setName,
      profileDialog.idx,
      values,
    );
    const body = singleProfileProbeRequest(
      profilesStore.config,
      staged,
      profileDialog.setName,
      profileDialog.idx,
    );
    if (!body) return;
    const resp = await profilesStore.probeRaw(body);
    if (!resp) return;
    mergeProviderScope(providerReports, resp);
    const p = resp.sets[0]?.profiles[0];
    if (p) {
      profileProbeReport = { status: p.status, detail: p.detail ?? null };
    }
  }

  // Parking dialog: single-profile form (see ProfileDialog). idx indexes
  // the draft's parked list in edit mode.
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
    store?: boolean;
    effort?: ReasoningEffort;
    modalities?: readonly Modality[];
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

  function onRemoveSetProfile(
    setName: string,
    profileIdx: number,
    profileId: string,
  ) {
    profilesStore.removeProfile(setName, profileIdx);
    clearProfileResult(profileReports, setName, profileId);
  }

  function onRemoveParked(idx: number, profileId: string) {
    const draft = profilesStore.draft;
    if (!draft) return;
    profilesStore.draft = replaceParkingProfiles(
      draft,
      (draft.parking ?? []).filter((_, i) => i !== idx),
    );
    profileReports.delete(`p:${profileId}`);
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
    store?: boolean;
    effort?: ReasoningEffort;
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
    const p = resp.sets[0]?.profiles[0];
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
    const p = resp.sets[0]?.profiles[0];
    if (p) {
      const id = draft.parking?.[idx]?.id ?? p.profile_id;
      profileReports.set(`p:${id}`, p);
    }
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
  // The source posture: a rejected enrollment token poisons the face
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
      {parkedLive}
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
                  parked: (localDisk.parking ?? []).length,
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
              onEdit={openProviderEdit}
              onAdd={openProviderNew}
              onRemove={onProviderRemove}
              {readOnly}
            />

            <SetsSection
              sets={setEntries}
              defaultName={profilesStore.draft.default}
              reports={profileReports}
              isProbing={profilesStore.isProbing}
              {dragOverSet}
              onCardDragStart={(fromSet, fromIdx) =>
                (drag = { area: "set", fromSet, fromIdx })}
              onCardDragEnd={clearDrag}
              onSetDragOver={(setName) => (dragOverSet = setName)}
              onSetDragLeave={(setName) =>
                (dragOverSet = setName === dragOverSet ? null : dragOverSet)}
              onSetDrop={onDropSet}
              {onTestSet}
              {onTestProfile}
              onEditSet={(setName) =>
                (setDialog = { open: true, name: setName })}
              onEditProfile={openProfileEdit}
              onRemoveSet={(setName) => onSetRemoveRequested(setName)}
              onSetDefault={(setName) => {
                const draft = profilesStore.draft;
                if (draft) profilesStore.draft = setDefaultSet(draft, setName);
              }}
              onAddSet={() => profilesStore.addSet()}
              onRemoveProfile={onRemoveSetProfile}
              {readOnly}
            />

            <ParkingSection
              parking={profilesStore.draft.parking ?? []}
              reports={profileReports}
              isProbing={profilesStore.isProbing}
              {dragOverParking}
              onCardDragStart={(fromIdx) =>
                (drag = { area: "parking", fromSet: null, fromIdx })}
              onCardDragEnd={clearDrag}
              onParkingDragOver={() => (dragOverParking = true)}
              onParkingDragLeave={() => (dragOverParking = false)}
              onParkingDrop={onDropParking}
              onTest={onTestParking}
              onEdit={openParkingEdit}
              onAdd={openParkingNew}
              onRemove={onRemoveParked}
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
  open={removeSetName !== null}
  title={manage_profiles_remove_set_confirm_title()}
  description={removeSetError
    ? removeSetError
    : removeSetUsers.length > 0
      ? manage_profiles_remove_set_confirm_users_desc({
          users: removeSetUsers.join(", "),
        })
      : manage_profiles_remove_set_confirm_desc()}
  confirmLabel={common_remove()}
  tone="danger"
  onConfirm={onSetRemoveConfirmed}
  onCancel={() => {
    removeSetName = null;
    removeSetUsers = [];
    removeSetError = null;
  }}
/>

<ProviderDialog
  open={providerDialog.open}
  mode={providerDialog.mode}
  provider={providerDialog.provider}
  existingIds={providerIds}
  onSave={onProviderSave}
  onCancel={() => (providerDialog.open = false)}
/>

<SetDialog
  open={setDialog.open}
  name={setDialog.name}
  description={profilesStore.draft?.sets[setDialog.name]?.description ?? null}
  allNames={setNames}
  profiles={profilesStore.draft?.sets[setDialog.name]?.profiles ?? []}
  {providerIds}
  onSave={onSetSave}
  onCancel={() => (setDialog.open = false)}
/>

<ProfileDialog
  open={parkingDialog.open}
  mode={parkingDialog.mode}
  description={manage_profiles_parking_dialog_desc()}
  title={parkingDialog.mode === "new"
    ? manage_profiles_parking_dialog_new_title()
    : manage_profiles_parking_dialog_edit_title()}
  profile={profilesStore.draft?.parking?.[parkingDialog.idx] ?? null}
  {providerIds}
  {occupiedIds}
  probeReport={parkingProbeReport}
  onSave={onParkingSave}
  onCancel={() => (parkingDialog.open = false)}
  onTest={onParkingTest}
  onRemove={parkingDialog.mode === "edit" ? onParkingRemove : null}
/>
<ProfileDialog
  open={profileDialog.open}
  mode="edit"
  title={manage_profiles_profile_dialog_edit_title()}
  description={manage_profiles_profile_dialog_desc()}
  profile={profilesStore.draft?.sets[profileDialog.setName]?.profiles[
    profileDialog.idx
  ] ?? null}
  {providerIds}
  {occupiedIds}
  probeReport={profileProbeReport}
  onSave={onProfileSave}
  onCancel={() => (profileDialog.open = false)}
  onTest={onProfileTest}
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
  open={removeProviderTarget !== null}
  title={manage_profiles_remove_provider()}
  description={manage_profiles_remove_provider_desc()}
  confirmLabel={common_remove()}
  tone="danger"
  onConfirm={onProviderRemoveConfirmed}
  onCancel={() => (removeProviderTarget = null)}
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
