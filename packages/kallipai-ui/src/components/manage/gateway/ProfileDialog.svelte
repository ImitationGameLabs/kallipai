<script lang="ts" module>
  // The gateway admin's unassigned-profile dialog: identity and the
  // anchoring collection (both creation only -- a row's collection never
  // moves), the referenced provider (frozen on edit -- the update
  // re-points nothing; the family is derived display), the model, and
  // the limits. Create hands back the full ProfileCreate shape; edit
  // hands back the ProfilePut replacement fields (the page sends a
  // created row as an unassigned row).
</script>

<script lang="ts">
  import FormDialog from "../../FormDialog.svelte";
  import ModalityTags from "../../ModalityTags.svelte";
  import ReqMark from "../../ReqMark.svelte";
  import {
    MODALITY_ORDER,
    EFFORT_OPTIONS,
    normalizeModalities,
  } from "../../../lib/compute.ts";
  import { parseProfileLimit } from "../../../lib/manage/gateway/profileLimits.ts";
  import type { AdminProfileRow } from "../../../lib/manage/gateway/client.ts";
  import type { Modality, ReasoningEffort } from "@kallipai/kallipai-client";
  import {
    manage_gateway_placement_collection_label,
    manage_gateway_provider_family,
    manage_gateway_provider_label,
    manage_gateway_provider_pool_empty,
    manage_gateway_unassigned_dialog_edit_title,
    manage_gateway_unassigned_dialog_new_title,
    user_gateway_profile_budget,
    user_gateway_profile_context,
    user_gateway_profile_effort,
    user_gateway_profile_effort_unset,
    user_gateway_profile_id,
    user_gateway_profile_limits,
    user_gateway_profile_modalities,
    user_gateway_profile_model,
    user_gateway_profile_rpm,
    user_gateway_profile_tpm,
  } from "../../../paraglide/messages.js";

  let {
    open,
    row,
    busy = false,
    error = null,
    onSave,
    onCancel,
    providers = [],
    collections = [],
    pinnedCollection = null,
  }: {
    open: boolean;
    /** The admin's provider pool (the provider dropdown). */
    providers?: readonly { provider_id: string; family: string }[];
    /** The admin's collections (the anchoring-collection dropdown). */
    collections?: readonly { name: string }[];
    /**
     * The collection a create takes, and whether the pick is the user's:
     * a name seeds the field and locks the picker (the open detail owns
     * the buffer), null leaves both blank and open (the list's entry).
     */
    pinnedCollection?: string | null;
    /** The row under edit (null = the create form). */
    row: {
      profile_id: string;
      collection_name: string;
      provider_id: string;
      family: string;
      model: string;
      max_context_window: number | null;
      effort: string | null;
      modalities: string[] | null;
      max_budget: number | null;
      tpm_limit: number | null;
      rpm_limit: number | null;
    } | null;
    busy?: boolean;
    error?: string | null;
    onSave: (result: {
      collection: string;
      profile_id: string;
      provider_id: string;
      model: string;
      max_context_window: number | null;
      effort: string | null;
      modalities: string[] | null;
      max_budget: number | null;
      tpm_limit: number | null;
      rpm_limit: number | null;
    }) => void;
    onCancel: () => void;
  } = $props();

  let idDraft = $state("");
  let collectionDraft = $state("");
  let providerDraft = $state("");
  let modelDraft = $state("");
  let effortDraft = $state("");
  let modalityDraft = $state<Modality[]>([]);
  let contextDraft = $state("");
  let budgetDraft = $state("");
  let tpmDraft = $state("");
  let rpmDraft = $state("");

  function reseed(): void {
    idDraft = row?.profile_id ?? "";
    // Three sources, in order: the row under edit, the collection a
    // pinned create belongs to, else blank. The catalog's first name
    // must never stand in for the user's pick.
    collectionDraft = row?.collection_name ?? pinnedCollection ?? "";
    providerDraft = row?.provider_id ?? "";
    modelDraft = row?.model ?? "";
    effortDraft = row?.effort ?? "";
    // A stored row can carry non-wire modality spellings; the tag
    // group shows the intersection (a save writes the visible
    // selection back).
    modalityDraft = normalizeModalitiesInit(row?.modalities ?? null);
    contextDraft = row?.max_context_window?.toString() ?? "";
    budgetDraft = row?.max_budget?.toString() ?? "";
    tpmDraft = row?.tpm_limit?.toString() ?? "";
    rpmDraft = row?.rpm_limit?.toString() ?? "";
  }

  const creating = $derived(row === null);
  const canSubmit = $derived(
    idDraft.trim() !== "" &&
      collectionDraft.trim() !== "" &&
      providerDraft.trim() !== "" &&
      modelDraft.trim() !== "",
  );

  // The family is derived display: it reads the selected provider's
  // row (create) or the edited row's own link (edit, where the
  // provider is frozen).
  const familyShown = $derived(
    providers.find((p) => p.provider_id === providerDraft)?.family ??
      row?.family ??
      "",
  );

  // The five effort options come from the shared EFFORT_OPTIONS
  // constant (compute.ts); it mirrors the ReasoningEffort union.
  /** The tag-group seeding: intersect the stored list with the wire
   * modalities (spellings outside the wire set are not representable). */
  function normalizeModalitiesInit(
    stored: readonly string[] | null,
  ): Modality[] {
    if (stored === null) return [];
    return MODALITY_ORDER.filter((m) => stored.includes(m));
  }

  const contextLimit = $derived(parseProfileLimit(contextDraft));
  const budgetLimit = $derived(parseProfileLimit(budgetDraft));
  const tpmLimit = $derived(parseProfileLimit(tpmDraft));
  const rpmLimit = $derived(parseProfileLimit(rpmDraft));

  const numbersValid = $derived(
    contextLimit !== undefined &&
      budgetLimit !== undefined &&
      tpmLimit !== undefined &&
      rpmLimit !== undefined,
  );

  function submit(): void {
    // Unreachable at runtime (canSubmit folds in numbersValid) and kept for
    // the type: parseProfileLimit answers number | null | undefined, the
    // payload wants number | null, and TypeScript cannot tell a boolean
    // derived from the four checks apart.
    if (
      contextLimit === undefined ||
      budgetLimit === undefined ||
      tpmLimit === undefined ||
      rpmLimit === undefined
    ) {
      return;
    }
    onSave({
      collection: collectionDraft.trim(),
      profile_id: idDraft.trim(),
      provider_id: providerDraft.trim(),
      model: modelDraft.trim(),
      max_context_window: contextLimit,
      effort: effortDraft.trim() === "" ? null : effortDraft.trim(),
      // text-only rides as null (the server default), the selection
      // as declared -- a save writes what the tags show.
      modalities: normalizeModalities(modalityDraft)?.slice() ?? null,
      max_budget: budgetLimit,
      tpm_limit: tpmLimit,
      rpm_limit: rpmLimit,
    });
  }
</script>

<FormDialog
  {open}
  title={creating
    ? manage_gateway_unassigned_dialog_new_title()
    : manage_gateway_unassigned_dialog_edit_title()}
  size="md"
  tall
  {busy}
  {error}
  canSubmit={canSubmit && numbersValid}
  onOpen={reseed}
  onSubmit={submit}
  {onCancel}
>
  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {user_gateway_profile_id()}
      <ReqMark />
    </span>
    <input
      class="input text-sm font-mono"
      bind:value={idDraft}
      disabled={!creating}
      aria-required="true"
    />
  </label>
  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {manage_gateway_placement_collection_label()}
      <ReqMark />
    </span>
    <select
      class="select text-sm font-mono"
      bind:value={collectionDraft}
      disabled={!creating || pinnedCollection !== null}
      aria-required="true"
    >
      {#if creating && collectionDraft === ""}
        <option value="" disabled></option>
      {/if}
      {#each collections as c (c.name)}
        <option value={c.name}>{c.name}</option>
      {/each}
    </select>
  </label>
  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {manage_gateway_provider_label()}
      <ReqMark />
    </span>
    <select
      class="select text-sm font-mono"
      bind:value={providerDraft}
      disabled={!creating}
      aria-required="true"
    >
      {#if providerDraft !== "" && !providers.some((p) => p.provider_id === providerDraft)}
        <!-- A stored row can reference a since-removed provider:
                keep it selectable so an edit never silently rewrites it. -->
        <option value={providerDraft}>{providerDraft}</option>
      {/if}
      {#each providers as p (p.provider_id)}
        <option value={p.provider_id}>{p.provider_id}</option>
      {/each}
    </select>
    {#if creating && providers.length === 0}
      <span class="text-xs opacity-60">
        {manage_gateway_provider_pool_empty()}
      </span>
    {/if}
    {#if familyShown !== ""}
      <span class="text-xs opacity-60">
        {manage_gateway_provider_family()}: {familyShown}
      </span>
    {/if}
  </label>
  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {user_gateway_profile_model()}
      <ReqMark />
    </span>
    <input
      class="input text-sm font-mono"
      bind:value={modelDraft}
      aria-required="true"
    />
  </label>
  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {user_gateway_profile_effort()}
    </span>
    <select class="select text-sm" bind:value={effortDraft}>
      {#if effortDraft !== "" && !EFFORT_OPTIONS.includes(effortDraft as ReasoningEffort)}
        <!-- A stored row can carry an effort outside the enum: keep
                it selectable so an edit never silently rewrites it. -->
        <option value={effortDraft}>{effortDraft}</option>
      {/if}
      <option value="">
        {user_gateway_profile_effort_unset()}
      </option>
      {#each EFFORT_OPTIONS as e (e)}
        <option value={e}>{e}</option>
      {/each}
    </select>
  </label>
  <ModalityTags
    label={user_gateway_profile_modalities()}
    selected={modalityDraft}
    onChange={(next) => (modalityDraft = [...next])}
  />

  <p class="text-xs font-medium uppercase opacity-60 tracking-wide">
    {user_gateway_profile_limits()}
  </p>
  <div class="grid grid-cols-2 gap-3">
    <label class="flex flex-col gap-1">
      <span class="text-xs font-medium">
        {user_gateway_profile_context()}
      </span>
      <input
        class="input text-sm"
        inputmode="numeric"
        bind:value={contextDraft}
      />
    </label>
    <label class="flex flex-col gap-1">
      <span class="text-xs font-medium">
        {user_gateway_profile_budget()}
      </span>
      <input
        class="input text-sm"
        inputmode="numeric"
        bind:value={budgetDraft}
      />
    </label>
    <label class="flex flex-col gap-1">
      <span class="text-xs font-medium">
        {user_gateway_profile_tpm()}
      </span>
      <input class="input text-sm" inputmode="numeric" bind:value={tpmDraft} />
    </label>
    <label class="flex flex-col gap-1">
      <span class="text-xs font-medium">
        {user_gateway_profile_rpm()}
      </span>
      <input class="input text-sm" inputmode="numeric" bind:value={rpmDraft} />
    </label>
  </div>
</FormDialog>
