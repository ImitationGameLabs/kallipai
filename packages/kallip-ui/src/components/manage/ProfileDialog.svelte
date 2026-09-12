<script lang="ts" module>
  // Single-profile create/edit dialog shared by the Profiles page's
  // parking section and its set-member profile cards: one field per
  // block, vertical (the ProviderDialog pattern), deliberately NOT
  // the SetDialog row editor (which edits a whole set at once).
  // Prop-driven (CreateRoomDialog pattern): the dialog never touches
  // a store; the page applies the result to its draft. Title and
  // description come from the call site — each context speaks with
  // its own i18n keys. In edit mode the id is locked — the id is the
  // profile's identity in the sets ∪ parking uniqueness rule, and
  // renaming would dangle probe reports keyed by it.
  import type { ProfileModel, ReasoningEffort } from "@kallipai/kallip-client";

  export interface ProfileDialogResult {
    readonly id: string;
    readonly endpoint: string;
    readonly model: string;
    readonly max_context_window: number;
    /** Pass-through: the form has no inputs for these; edits keep the
     * profile's declared values so a save never silently resets them. */
    readonly store?: boolean;
    readonly effort?: ReasoningEffort;
  }
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    common_cancel,
    common_save,
    manage_profiles_id_placeholder,
    manage_profiles_max_context_placeholder,
    manage_profiles_model_placeholder,
    manage_profiles_profile_dialog_endpoint_label,
    manage_profiles_profile_dialog_id_duplicate,
    manage_profiles_profile_dialog_invalid_window,
    manage_profiles_profile_dialog_id_label,
    manage_profiles_profile_dialog_max_context_label,
    manage_profiles_profile_dialog_model_label,
    manage_profiles_remove_profile,
    manage_profiles_test,
  } from "../../paraglide/messages.js";

  let {
    open,
    mode,
    profile = null,
    providerIds = [],
    occupiedIds = [],
    probeReport = null,
    onSave,
    onCancel,
    onTest = null,
    onRemove = null,
    title,
    description,
  }: {
    open: boolean;
    mode: "new" | "edit";
    /** The parked profile being edited (edit mode's initial values). */
    profile?: ProfileModel | null;
    /** Provider ids available in the draft (the endpoint dropdown). */
    providerIds?: string[];
    /** Every profile id visible in the draft, sets ∪ parking (the
     * new-mode duplicate check — advisory; PUT stays authoritative). */
    occupiedIds?: string[];
    /** Latest probe report for the in-form Test (rendered inline). */
    probeReport?: { status: string; detail: string | null } | null;
    onSave: (result: ProfileDialogResult) => void;
    onCancel: () => void;
    /** Probe the current form values without touching the draft. */
    onTest?: ((values: ProfileDialogResult) => void) | null;
    /** Edit mode's danger action; hide the zone when absent. */
    onRemove?: (() => void) | null;
    /** Dialog title text (the call site picks the i18n keys). */
    title: string;
    /** sr-only dialog description text (call-site i18n choice). */
    description: string;
  } = $props();

  // Field drafts, reset on each open transition (plain latch, no
  // self-trigger — same latch as ProviderDialog/SetDialog).
  let id = $state("");
  let endpoint = $state("");
  let model = $state("");
  let maxContext = $state("128000");
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      id = profile?.id ?? "";
      endpoint = profile?.endpoint ?? providerIds[0] ?? "";
      model = profile?.model ?? "";
      maxContext = String(profile?.max_context_window ?? 128000);
    }
    lastOpen = open;
  });

  const trimmedId = $derived(id.trim());
  const trimmedModel = $derived(model.trim());
  const duplicateId = $derived(
    mode === "new" && occupiedIds.includes(trimmedId),
  );
  const validWindow = $derived(
    Number.isInteger(Number(maxContext)) && Number(maxContext) > 0,
  );
  const canSubmit = $derived(
    trimmedId.length > 0 &&
      !duplicateId &&
      endpoint !== "" &&
      trimmedModel.length > 0 &&
      validWindow,
  );
  // The probe request carries id/endpoint/model only, so an invalid
  // window does not block Test.
  const canTest = $derived(
    onTest !== null &&
      trimmedId.length > 0 &&
      endpoint !== "" &&
      trimmedModel.length > 0,
  );

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open) onCancel();
  }

  function submit(): void {
    if (!canSubmit) return;
    onSave({
      id: trimmedId,
      endpoint,
      model: trimmedModel,
      max_context_window: Number(maxContext),
      store: profile?.store,
      effort: profile?.effort,
    });
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-md p-6 flex flex-col gap-4"
      >
        <Dialog.Title class="text-lg font-semibold">
          {title}
        </Dialog.Title>
        <Dialog.Description class="sr-only">
          {description}
        </Dialog.Description>

        <form
          class="flex flex-col gap-4"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium">
              {manage_profiles_profile_dialog_id_label()}
              {#if mode === "new"}
                <span class="text-error-500 dark:text-error-400">*</span>
              {/if}
            </span>
            <input
              class="input text-sm font-mono"
              placeholder={manage_profiles_id_placeholder()}
              bind:value={id}
              disabled={mode === "edit"}
              required
            />
            {#if duplicateId}
              <span class="text-xs text-error-500 dark:text-error-400"
                >{manage_profiles_profile_dialog_id_duplicate()}</span
              >
            {/if}
          </label>

          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium">
              {manage_profiles_profile_dialog_endpoint_label()}
              <span class="text-error-500 dark:text-error-400">*</span>
            </span>
            <select class="select text-sm" bind:value={endpoint}>
              {#each providerIds as eid (eid)}
                <option value={eid}>{eid}</option>
              {/each}
            </select>
          </label>

          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium">
              {manage_profiles_profile_dialog_model_label()}
              <span class="text-error-500 dark:text-error-400">*</span>
            </span>
            <input
              class="input text-sm font-mono"
              placeholder={manage_profiles_model_placeholder()}
              bind:value={model}
              required
            />
          </label>

          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium">
              {manage_profiles_profile_dialog_max_context_label()}
              <span class="text-error-500 dark:text-error-400">*</span>
            </span>
            <input
              class="input text-sm font-mono"
              inputmode="numeric"
              placeholder={manage_profiles_max_context_placeholder()}
              bind:value={maxContext}
              required
            />
            {#if !validWindow}
              <span class="text-xs text-error-500 dark:text-error-400"
                >{manage_profiles_profile_dialog_invalid_window()}</span
              >
            {/if}
          </label>

          {#if probeReport}
            <div class="border-t border-surface-300 pt-2 text-xs">
              <span class="font-mono">{probeReport.status}</span>
              {#if probeReport.detail}
                <span class="opacity-60 ml-2 font-mono break-all">
                  {probeReport.detail}
                </span>
              {/if}
            </div>
          {/if}

          {#if mode === "edit" && onRemove}
            <div class="border-t border-surface-300 pt-3">
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
                onclick={onRemove}
              >
                {manage_profiles_remove_profile()}
              </button>
            </div>
          {/if}

          <div class="flex gap-2">
            {#if onTest}
              <button
                type="button"
                class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"
                disabled={!canTest}
                onclick={() =>
                  onTest?.({
                    id: trimmedId,
                    endpoint,
                    model: trimmedModel,
                    max_context_window: Number(maxContext),
                  })}
              >
                {manage_profiles_test()}
              </button>
            {/if}
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
              disabled={!canSubmit}
            >
              {common_save()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
