<script lang="ts" module>
  // Catalog collection create/edit dialog: the name (the registry key,
  // writable only on create) plus the description. The dialog holds a
  // local draft seeded on each open transition and hands the shape
  // back; the page lands it as one POST or PATCH through the store's
  // wraps (the set membership is not a collection attribute: sets
  // arrive only through the nested creation door).
  import type { AdminCollectionRow } from "../../../lib/manage/gateway/client.ts";

  export interface GatewayCollectionDialogResult {
    readonly name: string;
    readonly description: string;
  }
</script>

<script lang="ts">
  import FormDialog from "../../FormDialog.svelte";
  import ReqMark from "../../ReqMark.svelte";
  import {
    manage_gateway_collection_dialog_create_title,
    manage_gateway_collection_dialog_edit_title,
    manage_profiles_set_dialog_description_label,
    manage_profiles_set_dialog_name_invalid,
    manage_profiles_set_dialog_name_label,
    manage_profiles_set_dialog_name_taken,
  } from "../../../paraglide/messages.js";

  let {
    open,
    collection,
    existingNames = [],
    busy = false,
    error = null,
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The collection under edit (null = the create shape). */
    collection: AdminCollectionRow | null;
    /** The existing collection names: a collision disables the save. */
    existingNames?: readonly string[];
    busy?: boolean;
    error?: string | null;
    onSave: (result: GatewayCollectionDialogResult) => void;
    onCancel: () => void;
  } = $props();

  // The local draft, reseeded by the shell on each open transition.
  let nameDraft = $state("");
  let descriptionDraft = $state("");

  function reseed(): void {
    nameDraft = collection?.name ?? "";
    descriptionDraft = collection?.description ?? "";
  }

  // Narrower than the server's rule (the gateway refuses empty names,
  // '/' and ':'); a duplicate name is refused server-side. Only the
  // create shape validates the name -- the edit face never renames.
  const nameError = $derived(
    collection !== null
      ? null
      : !/^[A-Za-z0-9_-]+$/.test(nameDraft)
        ? manage_profiles_set_dialog_name_invalid()
        : existingNames.includes(nameDraft.trim())
          ? manage_profiles_set_dialog_name_taken()
          : null,
  );
  const canSubmit = $derived(nameError === null && !busy);

  function submit(): void {
    onSave({ name: nameDraft.trim(), description: descriptionDraft });
  }
</script>

{#snippet titleExtra()}
  {#if collection !== null}
    <span class="font-mono opacity-80">{collection.name}</span>
  {/if}
{/snippet}

<FormDialog
  {open}
  title={collection === null
    ? manage_gateway_collection_dialog_create_title()
    : manage_gateway_collection_dialog_edit_title()}
  {titleExtra}
  size="xl"
  tall
  {busy}
  {error}
  {canSubmit}
  onOpen={reseed}
  onSubmit={submit}
  {onCancel}
>
  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {manage_profiles_set_dialog_name_label()}
      <ReqMark />
    </span>
    <input
      class="input text-sm font-mono"
      bind:value={nameDraft}
      disabled={collection !== null}
      aria-required="true"
    />
    {#if nameError}
      <span class="text-xs text-error-500 dark:text-error-400">
        {nameError}
      </span>
    {/if}
  </label>

  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {manage_profiles_set_dialog_description_label()}
    </span>
    <input class="input text-sm" bind:value={descriptionDraft} />
  </label>
</FormDialog>
