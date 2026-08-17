<script lang="ts" module>
  // Endpoint create/edit dialog for the Profiles page's endpoint cards.
  // Prop-driven (CreateRoomDialog pattern): the dialog never touches a store —
  // the owning page applies the result to its draft. In edit mode an empty key
  // means "keep the draft's existing (masked) key", so an untouched round-trip
  // leaves isDirty alone; the id is locked because renaming would dangle every
  // profile referencing it (the PUT's validate_endpoints rejects that).
  import type { ProfileEndpoint } from "@kallipai/kallip-client";

  export interface EndpointDialogResult {
    readonly id: string;
    readonly family: string;
    readonly baseUrl: string | null;
    /** null = keep the existing draft key (edit mode only). */
    readonly apiKey: string | null;
  }
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    common_cancel,
    common_save,
    manage_profiles_endpoint_api_key_hint_edit,
    manage_profiles_endpoint_api_key_label,
    manage_profiles_endpoint_api_key_required,
    manage_profiles_endpoint_base_url_hint,
    manage_profiles_endpoint_base_url_label,
    manage_profiles_endpoint_dialog_desc,
    manage_profiles_endpoint_dialog_edit_title,
    manage_profiles_endpoint_dialog_new_title,
    manage_profiles_endpoint_family_label,
    manage_profiles_endpoint_id_duplicate,
    manage_profiles_endpoint_id_hint,
    manage_profiles_endpoint_id_label,
    manage_profiles_remove_endpoint,
  } from "../../paraglide/messages.js";

  let {
    open,
    mode,
    endpoint = null,
    existingIds = [],
    onSave,
    onCancel,
    onRemove = null,
  }: {
    open: boolean;
    mode: "new" | "edit";
    /** The draft endpoint being edited (edit mode's initial values). */
    endpoint?: ProfileEndpoint | null;
    /** Ids already present in the draft (new-mode duplicate check). */
    existingIds?: string[];
    onSave: (result: EndpointDialogResult) => void;
    onCancel: () => void;
    /** Edit mode's danger action; hide the zone when absent. */
    onRemove?: (() => void) | null;
  } = $props();

  // Families the tagma backend knows how to build.
  const FAMILIES = ["deepseek", "openai-compatible"];

  // Drafts, reset on each open transition (plain latch, no self-trigger).
  let id = $state("");
  let family = $state(FAMILIES[0]!);
  let baseUrl = $state("");
  let apiKey = $state("");
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      id = mode === "edit" && endpoint ? endpoint.id : "";
      family = endpoint?.family ?? FAMILIES[0]!;
      baseUrl = endpoint?.base_url ?? "";
      apiKey = "";
    }
    lastOpen = open;
  });

  const trimmedId = $derived(id.trim());
  const trimmedKey = $derived(apiKey.trim());
  const duplicateId = $derived(
    mode === "new" && existingIds.includes(trimmedId),
  );
  const canSubmit = $derived(
    trimmedId.length > 0 &&
      !duplicateId &&
      (mode === "edit" || trimmedKey.length > 0),
  );

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open) onCancel();
  }

  function submit(): void {
    if (!canSubmit) return;
    onSave({
      id: trimmedId,
      family,
      baseUrl: baseUrl.trim() === "" ? null : baseUrl.trim(),
      apiKey:
        mode === "new" || trimmedKey !== "" ? trimmedKey : null,
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
          {mode === "new"
            ? manage_profiles_endpoint_dialog_new_title()
            : manage_profiles_endpoint_dialog_edit_title()}
        </Dialog.Title>
        <Dialog.Description class="sr-only">
          {manage_profiles_endpoint_dialog_desc()}
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
              {manage_profiles_endpoint_id_label()}
              {#if mode === "new"}
                <span class="text-error-500 dark:text-error-400">*</span>
              {/if}
            </span>
            <input
              class="input text-sm font-mono"
              bind:value={id}
              disabled={mode === "edit"}
              required
            />
            <span class="text-xs opacity-60">
              {manage_profiles_endpoint_id_hint()}
            </span>
            {#if duplicateId}
              <span
                class="text-xs text-error-500 dark:text-error-400"
              >{manage_profiles_endpoint_id_duplicate()}</span>
            {/if}
          </label>

          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium">
              {manage_profiles_endpoint_family_label()}
            </span>
            <select class="select text-sm" bind:value={family}>
              {#each FAMILIES as f (f)}
                <option value={f}>{f}</option>
              {/each}
            </select>
          </label>

          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium">
              {manage_profiles_endpoint_base_url_label()}
            </span>
            <input
              class="input text-sm font-mono"
              bind:value={baseUrl}
            />
            <span class="text-xs opacity-60">
              {manage_profiles_endpoint_base_url_hint()}
            </span>
          </label>

          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium">
              {manage_profiles_endpoint_api_key_label()}
              {#if mode === "new"}
                <span class="text-error-500 dark:text-error-400">*</span>
              {/if}
            </span>
            <input
              class="input text-sm font-mono"
              type="password"
              autocomplete="off"
              bind:value={apiKey}
            />
            <span class="text-xs opacity-60">
              {#if mode === "new"}
                {manage_profiles_endpoint_api_key_required()}
              {:else if endpoint}
                {manage_profiles_endpoint_api_key_hint_edit({
                  mask: endpoint.api_key ?? "",
                })}
              {/if}
            </span>
          </label>

          {#if mode === "edit" && onRemove}
            <div class="border-t border-surface-300 pt-3">
              <button
                type="button"
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-error-500"
                onclick={onRemove}
              >
                {manage_profiles_remove_endpoint()}
              </button>
            </div>
          {/if}

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
