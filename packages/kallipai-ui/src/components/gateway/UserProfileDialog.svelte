<script lang="ts" module>
  // Profile edit dialog: identity (creation only), the model, and the
  // optional limits. PUT is a full-field replacement, so the dialog
  // hands back every field each save; the blank numeric fields carry
  // no limit. Modalities are the shared tag group (empty = text only,
  // the server default rides as null).
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import ModalityTags from "../ModalityTags.svelte";
  import ReqMark from "../ReqMark.svelte";
  import {
    MODALITY_ORDER,
    EFFORT_OPTIONS,
    normalizeModalities,
  } from "../../lib/manage/compute.ts";
  import type { Modality, ReasoningEffort } from "@kallipai/kallipai-client";
  import {
    common_cancel,
    common_save,
    user_gateway_profile_add,
    user_gateway_profile_budget,
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
  }: {
    open: boolean;
    /** The caller's provider pool (the provider dropdown). */
    providers?: readonly { provider_id: string }[];
    /** The row under edit (null = the create form). */
    row: {
      profile_id: string;
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
  let providerDraft = $state("");
  let modelDraft = $state("");
  let effortDraft = $state("");
  let modalityDraft = $state<Modality[]>([]);
  let contextDraft = $state("");
  let budgetDraft = $state("");
  let tpmDraft = $state("");
  let rpmDraft = $state("");
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      idDraft = row?.profile_id ?? "";
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
    lastOpen = open;
  });

  const creating = $derived(row === null);
  const canSubmit = $derived(
    idDraft.trim() !== "" &&
      providerDraft.trim() !== "" &&
      modelDraft.trim() !== "",
  );

  /** Blank = no limit; a non-numeric value keeps the save disabled
   * (the server would refuse it anyway). */
  function numberOrNull(raw: string): number | null {
    const trimmed = raw.trim();
    if (trimmed === "") return null;
    const n = Number(trimmed);
    return Number.isFinite(n) ? n : null;
  }

  function numeric(raw: string): boolean {
    const trimmed = raw.trim();
    return trimmed === "" || Number.isFinite(Number(trimmed));
  }

  const numbersValid = $derived(
    numeric(contextDraft) &&
      numeric(budgetDraft) &&
      numeric(tpmDraft) &&
      numeric(rpmDraft),
  );

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }

  /** The tag-group seeding: intersect the stored list with the wire
   * modalities (spellings outside the wire set are not representable). */
  function normalizeModalitiesInit(
    stored: readonly string[] | null,
  ): Modality[] {
    if (stored === null) return [];
    return MODALITY_ORDER.filter((m) => stored.includes(m));
  }

  function submit(): void {
    if (!canSubmit || !numbersValid || busy) return;
    onSave({
      profile_id: idDraft.trim(),
      provider_id: providerDraft.trim(),
      model: modelDraft.trim(),
      max_context_window: numberOrNull(contextDraft),
      effort: effortDraft.trim() === "" ? null : effortDraft.trim(),
      // text-only rides as null (the server default), the selection
      // as declared -- a save writes what the tags show.
      modalities: normalizeModalities(modalityDraft)?.slice() ?? null,
      max_budget: numberOrNull(budgetDraft),
      tpm_limit: numberOrNull(tpmDraft),
      rpm_limit: numberOrNull(rpmDraft),
    });
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-md p-6 flex flex-col gap-3 max-h-[85vh] overflow-y-auto"
      >
        <Dialog.Title class="text-lg font-semibold">
          {creating
            ? user_gateway_profile_add()
            : user_gateway_profile_edit_title()}
        </Dialog.Title>

        <form
          class="flex flex-col gap-3"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
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
              <input
                class="input text-sm"
                inputmode="numeric"
                bind:value={tpmDraft}
              />
            </label>
            <label class="flex flex-col gap-1">
              <span class="text-xs font-medium">
                {user_gateway_profile_rpm()}
              </span>
              <input
                class="input text-sm"
                inputmode="numeric"
                bind:value={rpmDraft}
              />
            </label>
          </div>

          {#if error}
            <p class="text-sm text-error-500" role="alert">{error}</p>
          {/if}

          <div class="flex gap-2">
            <button
              type="button"
              class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"
              onclick={onCancel}
            >
              {common_cancel()}
            </button>
            <button
              type="submit"
              class="btn flex-1 preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110"
              disabled={!canSubmit || !numbersValid || busy}
            >
              {common_save()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
