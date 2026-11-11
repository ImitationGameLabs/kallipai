<script lang="ts" module>
  // Profile edit dialog: identity and the anchoring collection (both
  // creation only -- a row's collection never moves), the model, and the
  // limits. PUT is a full-field replacement, so the dialog hands back
  // every field each save; the blank numeric fields carry no limit.
  // Modalities are the shared tag group (empty = text only, the server
  // default rides as null).
</script>

<script lang="ts">
  import FormDialog from "../FormDialog.svelte";
  import ModalityTags from "../ModalityTags.svelte";
  import ReqMark from "../ReqMark.svelte";
  import {
    MODALITY_ORDER,
    EFFORT_OPTIONS,
    normalizeModalities,
  } from "../../lib/compute.ts";
  import { parseProfileLimit } from "../../lib/manage/gateway/profileLimits.ts";
  import type { Modality, ReasoningEffort } from "@kallipai/kallipai-client";
  import {
    user_gateway_profile_add,
    user_gateway_profile_budget,
    user_gateway_profile_collection,
    user_gateway_profile_context,
    user_gateway_profile_edit_title,
    user_gateway_profile_effort,
    user_gateway_profile_id,
    user_gateway_profile_limits,
    user_gateway_profile_modalities,
    user_gateway_profile_model,
    user_gateway_profile_provider,
    user_gateway_profile_provider_pool_empty,
    user_gateway_profile_rpm,
    user_gateway_profile_tpm,
    user_gateway_profile_effort_unset,
  } from "../../paraglide/messages.js";

  let {
    open,
    row,
    busy = false,
    error = null,
    onSave,
    onCancel,
    providers = [],
    collections = [],
  }: {
    open: boolean;
    /** The caller's provider pool (the provider dropdown). */
    providers?: readonly { provider_id: string }[];
    /** The caller's collections (the anchoring-collection dropdown). */
    collections?: readonly { name: string }[];
    /** The row under edit (null = the create form). */
    row: {
      profile_id: string;
      collection_name: string;
      provider_id: string;
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
    // Blank until picked: the first collection in name order is not a
    // sensible default for a row the caller is about to place.
    collectionDraft = row?.collection_name ?? "";
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

  /** The tag-group seeding: intersect the stored list with the wire
   * modalities (spellings outside the wire set are not representable). */
  function normalizeModalitiesInit(
    stored: readonly string[] | null,
  ): Modality[] {
    if (stored === null) return [];
    return MODALITY_ORDER.filter((m) => stored.includes(m));
  }

  function submit(): void {
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
    ? user_gateway_profile_add()
    : user_gateway_profile_edit_title()}
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
      {user_gateway_profile_collection()}
      <ReqMark />
    </span>
    <select
      class="select text-sm font-mono"
      bind:value={collectionDraft}
      disabled={!creating}
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
      {user_gateway_profile_provider()}
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
        {user_gateway_profile_provider_pool_empty()}
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
