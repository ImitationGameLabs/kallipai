<script lang="ts" module>
  // Tier create/edit dialog for the Profiles page's tier containers: a
  // compact row form for the tier's profiles (id / endpoint / model /
  // max context), rows removable, "+ add profile" at the bottom.
  // Prop-driven (CreateRoomDialog pattern): the dialog never touches a
  // store; on save it hands the full profile list back and the page applies
  // it (replaceTierProfiles / addTier+replace).
  import type { ProfileModel } from "@kallipai/kallip-client";

  export interface TierDialogRow {
    readonly id: string;
    readonly endpoint: string;
    readonly model: string;
    readonly max_context_window: number;
  }
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    common_cancel,
    common_save,
    manage_profiles_add_profile,
    manage_profiles_max_context_placeholder,
    manage_profiles_id_placeholder,
    manage_profiles_model_placeholder,
    manage_profiles_remove_profile,
    manage_profiles_tier_dialog_desc,
    manage_profiles_tier_dialog_edit_title,
    manage_profiles_tier_dialog_max_context_label,
    manage_profiles_tier_dialog_new_title,
    manage_profiles_tier_dialog_id_label,
    manage_profiles_tier_dialog_endpoint_label,
    manage_profiles_tier_dialog_model_label,
    manage_profiles_tier_dialog_no_endpoints,
  } from "../../paraglide/messages.js";

  let {
    open,
    mode,
    profiles = [],
    tierIdx = 0,
    endpointIds = [],
    onSave,
    onCancel,
  }: {
    open: boolean;
    mode: "new" | "edit";
    /** The tier's current profiles (edit mode's initial rows). */
    profiles?: readonly ProfileModel[];
    /** Tier index for the edit title (0-based, shown 1-based). */
    tierIdx?: number;
    /** Endpoint ids available in the draft (the endpoint dropdown). */
    endpointIds?: string[];
    onSave: (rows: TierDialogRow[]) => void;
    onCancel: () => void;
  } = $props();

  interface Row {
    id: string;
    endpoint: string;
    model: string;
    maxContext: string;
  }

  // Rows, reset on each open transition (plain latch, no self-trigger).
  let rows = $state<Row[]>([]);
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      rows = mode === "edit"
        ? profiles.map((p) => ({
            id: p.id,
            endpoint: p.endpoint,
            model: p.model,
            maxContext: String(p.max_context_window),
          }))
        : [];
      if (mode === "new" && rows.length === 0) rows = [blankRow()];
    }
    lastOpen = open;
  });

  function blankRow(): Row {
    return {
      id: "",
      endpoint: endpointIds[0] ?? "",
      model: "",
      maxContext: "128000",
    };
  }

  const duplicateIds = $derived(
    new Set(rows.map((r) => r.id.trim())).size !== rows.length
  );
  const canSubmit = $derived(
    rows.length > 0 &&
      !duplicateIds &&
      rows.every(
        (r) =>
          r.id.trim() !== "" &&
          r.endpoint !== "" &&
          r.model.trim() !== "" &&
          Number.isInteger(Number(r.maxContext)) &&
          Number(r.maxContext) > 0,
      ),
  );

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open) onCancel();
  }

  function submit(): void {
    if (!canSubmit) return;
    onSave(
      rows.map((r) => ({
        id: r.id.trim(),
        endpoint: r.endpoint,
        model: r.model.trim(),
        max_context_window: Number(r.maxContext),
      })),
    );
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-2xl p-6 flex flex-col gap-4 max-h-[85vh] overflow-y-auto"
      >
        <Dialog.Title class="text-lg font-semibold">
          {mode === "new"
            ? manage_profiles_tier_dialog_new_title()
            : manage_profiles_tier_dialog_edit_title({ index: tierIdx + 1 })}
        </Dialog.Title>
        <Dialog.Description class="sr-only">
          {manage_profiles_tier_dialog_desc()}
        </Dialog.Description>

        <form
          class="flex flex-col gap-4"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          {#if endpointIds.length === 0}
            <p class="text-xs text-error-500 dark:text-error-400">
              {manage_profiles_tier_dialog_no_endpoints()}
            </p>
          {/if}

          <div class="flex flex-col gap-3">
            {#each rows as row, i (i)}
              <div class="grid grid-cols-[1fr_1fr_1fr_5rem_2.25rem] items-start gap-2">
                <label class="flex flex-col gap-1">
                  <span class="text-xs font-medium">
                    {i === 0 ? manage_profiles_tier_dialog_id_label() : ""}
                  </span>
                  <input
                    class="input text-sm font-mono"
                    placeholder={manage_profiles_id_placeholder()}
                    bind:value={row.id}
                  />
                </label>
                <label class="flex flex-col gap-1">
                  <span class="text-xs font-medium">
                    {i === 0 ? manage_profiles_tier_dialog_endpoint_label() : ""}
                  </span>
                  <select class="select text-sm" bind:value={row.endpoint}>
                    {#each endpointIds as eid (eid)}
                      <option value={eid}>{eid}</option>
                    {/each}
                  </select>
                </label>
                <label class="flex flex-col gap-1">
                  <span class="text-xs font-medium">
                    {i === 0 ? manage_profiles_tier_dialog_model_label() : ""}
                  </span>
                  <input
                    class="input text-sm font-mono"
                    placeholder={manage_profiles_model_placeholder()}
                    bind:value={row.model}
                  />
                </label>
                <label class="flex flex-col gap-1">
                  <span class="text-xs font-medium">
                    {i === 0 ? manage_profiles_tier_dialog_max_context_label() : ""}
                  </span>
                  <input
                    class="input text-sm font-mono"
                    inputmode="numeric"
                    placeholder={manage_profiles_max_context_placeholder()}
                    bind:value={row.maxContext}
                  />
                </label>
                <button
                  type="button"
                  class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500 mt-6"
                  aria-label={manage_profiles_remove_profile()}
                  onclick={() => (rows = rows.filter((_, ri) => ri !== i))}
                >
                  ✕
                </button>
              </div>
            {/each}
            <button
              type="button"
              class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500 self-start"
              onclick={() => (rows = [...rows, blankRow()])}
            >
              {manage_profiles_add_profile()}
            </button>
          </div>

          <div class="flex justify-end gap-2">
            <button
              type="button"
              class="btn preset-outlined-surface-500"
              onclick={onCancel}
            >
              {common_cancel()}
            </button>
            <button
              type="submit"
              class="btn preset-filled-primary-500 text-on-primary-500"
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
