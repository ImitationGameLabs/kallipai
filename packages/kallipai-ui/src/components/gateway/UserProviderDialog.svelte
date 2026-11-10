<script lang="ts" module>
  // Provider edit dialog: the row's identity (creation only) and
  // shape, with an optional seed key (the credential write lives in
  // its own dialog). The dialog holds a local draft seeded on each
  // open transition; the page lands it as one create or update.
</script>

<script lang="ts">
  import FormDialog from "../FormDialog.svelte";
  import SecretInput from "../SecretInput.svelte";
  import ReqMark from "../ReqMark.svelte";
  import {
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
  function reseed(): void {
    idDraft = row?.provider_id ?? "";
    familyDraft = row?.family ?? FAMILIES[0]!;
    baseUrlDraft = row?.base_url ?? "";
    apiKeyDraft = "";
  }

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

  function submit(): void {
    onSave({
      provider_id: idDraft.trim(),
      family: familyDraft.trim(),
      base_url: baseUrlDraft.trim() === "" ? null : baseUrlDraft.trim(),
      api_key: apiKeyDraft === "" ? null : apiKeyDraft,
    });
  }
</script>

<FormDialog
  {open}
  title={creating
    ? user_gateway_provider_add()
    : user_gateway_provider_edit_title()}
  {busy}
  {error}
  {canSubmit}
  onOpen={reseed}
  onSubmit={submit}
  {onCancel}
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
</FormDialog>
