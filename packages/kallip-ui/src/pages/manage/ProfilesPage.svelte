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
  import { refreshParkedLive as fetchParkedLive } from "../../lib/manage/parkedLive.ts";
  import { displayError } from "../../lib/manage/errors.ts";
  import { KallipError } from "@kallipai/kallip-common";
  import { SvelteMap } from "svelte/reactivity";
  import ProfilesToolbar from "../../components/manage/ProfilesToolbar.svelte";
  import ProvidersSection from "../../components/manage/ProvidersSection.svelte";
  import SetsSection from "../../components/manage/SetsSection.svelte";
  import ParkingSection from "../../components/manage/ParkingSection.svelte";
  import ConfirmDialog from "../../components/ConfirmDialog.svelte";
  import ProviderDialog from "../../components/manage/ProviderDialog.svelte";
  import SetDialog from "../../components/manage/SetDialog.svelte";
  import ParkingDialog from "../../components/manage/ParkingDialog.svelte";
  import {
    moveProfile,
    moveFromParking,
    moveToParking,
    replaceSetProfiles,
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
    ProfileProviderProbeReport,
    ProfileModelProbeReport,
    ProfileSet,
    ReasoningEffort,
  } from "@kallipai/kallip-client";
  import {
    common_remove,
    common_save,
    manage_profiles_apply,
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

  async function onConfirmDanglingSave() {
    await profilesStore.save(true).catch(() => {});
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
        if (!(e instanceof KallipError && e.api.status === 404)) {
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
    store?: boolean;
    effort?: ReasoningEffort;
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
    />

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
        onEditSet={(setName) => (setDialog = { open: true, name: setName })}
        onRemoveSet={(setName) => onSetRemoveRequested(setName)}
        onSetDefault={(setName) => {
          const draft = profilesStore.draft;
          if (draft) profilesStore.draft = setDefaultSet(draft, setName);
        }}
        onAddSet={() => profilesStore.addSet()}
        onRemoveProfile={onRemoveSetProfile}
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
      />
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

<ConfirmDialog
  open={profilesStore.pendingDangling !== null}
  busy={profilesStore.isSaving || profilesStore.saveBlocked}
  error={profilesStore.saveBlocked ? profilesStore.error : null}
  title={manage_profiles_dangling_title()}
  description={manage_profiles_dangling_desc({
    count: profilesStore.pendingDangling?.length ?? 0,
    list: profilesStore.pendingDangling?.join("\n") ?? "",
  })}
  confirmLabel={common_save()}
  tone="primary"
  onConfirm={onConfirmDanglingSave}
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
