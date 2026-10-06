<script lang="ts" module>
  // Provider edit dialog: the row's identity (creation only) and
  // shape, with an optional seed key (the credential write lives in
  // its own dialog). The dialog holds a local draft seeded on each
  // open transition; the page lands it as one create or update.
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import SecretInput from "../SecretInput.svelte";
  import ReqMark from "../ReqMark.svelte";
  import {
    common_cancel,
    common_save,
    user_gateway_provider_add,
    user_gateway_provider_base,
    user_gateway_provider_edit_title,
    user_gateway_provider_family,
    user_gateway_provider_id,
    user_gateway_provider_key,
  } from "../../paraglide/messages.js";
  import { MODEL_PROVIDER_FAMILIES } from "../../lib/providerFamilies.ts";

  let {
    open,
    row,
    busy = false,
    error = null,
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The row under edit (null = the create form). */
    row: {
      provider_id: string;
      family: string;
      base_url: string | null;
    } | null;
    busy?: boolean;
    error?: string | null;
    onSave: (result: {
      provider_id: string;
      family: string;
      base_url: string | null;
      api_key: string | null;
    }) => void;
    onCancel: () => void;
  } = $props();

  const FAMILIES = MODEL_PROVIDER_FAMILIES;

  // The local draft. The id is editable only on creation; the key field
  // starts empty on edit (blank = leave the stored credential alone).
  let idDraft = $state("");
  let familyDraft = $state("");
  let baseUrlDraft = $state("");
  let apiKeyDraft = $state("");
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      idDraft = row?.provider_id ?? "";
      familyDraft = row?.family ?? FAMILIES[0]!;
      baseUrlDraft = row?.base_url ?? "";
      apiKeyDraft = "";
    }
    lastOpen = open;
  });

  const creating = $derived(row === null);
  const canSubmit = $derived(
    idDraft.trim() !== "" && familyDraft.trim() !== "",
  );

  // A stored row can carry a family the enum grew away from; keep it
  // selectable so an edit never silently rewrites it.
  const familyKept = $derived(
    familyDraft !== "" &&
      !(FAMILIES as readonly string[]).includes(familyDraft),
  );

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }

  function submit(): void {
    if (!canSubmit || busy) return;
    onSave({
      provider_id: idDraft.trim(),
      family: familyDraft.trim(),
      base_url: baseUrlDraft.trim() === "" ? null : baseUrlDraft.trim(),
      api_key: apiKeyDraft === "" ? null : apiKeyDraft,
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
          {creating
            ? user_gateway_provider_add()
            : user_gateway_provider_edit_title()}
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
              {user_gateway_provider_id()}
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
              {user_gateway_provider_family()}
              <ReqMark />
            </span>
            <select
              class="select text-sm"
              bind:value={familyDraft}
              aria-required="true"
            >
              {#if familyKept}
                <option value={familyDraft}>{familyDraft}</option>
              {/if}
              {#each FAMILIES as f (f)}
                <option value={f}>{f}</option>
              {/each}
            </select>
          </label>
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {user_gateway_provider_base()}
            </span>
            <input class="input text-sm" bind:value={baseUrlDraft} />
          </label>
          {#if creating}
            <label class="flex flex-col gap-1">
              <span class="text-xs font-medium">
                {user_gateway_provider_key()}
              </span>
              <SecretInput bind:value={apiKeyDraft} />
            </label>
          {/if}

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
              disabled={!canSubmit || busy}
            >
              {common_save()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
