<script lang="ts" module>
  // Catalog set creation dialog: the name (the registry key), the
  // optional description. The dialog opens from inside a collection
  // detail (the collection is pre-bound, not a pick here) and holds
  // a local draft seeded on each open transition; the page lands
  // the fields as one POST through the store's add wrap. Duplicate
  // names disable the save (the name is the registry key, so a
  // collision is refused).
</script>

<script lang="ts">
  import FormDialog from "../../FormDialog.svelte";
  import ReqMark from "../../ReqMark.svelte";
  import {
    manage_gateway_set_add,
    manage_profiles_set_dialog_description_label,
    manage_profiles_set_dialog_name_invalid,
    manage_profiles_set_dialog_name_label,
    manage_profiles_set_dialog_name_taken,
  } from "../../../paraglide/messages.js";

  let {
    open,
    names = [],
    busy = false,
    error = null,
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The existing set names: a collision disables the save. */
    names: readonly string[];
    busy?: boolean;
    error?: string | null;
    onSave: (result: { name: string; description: string }) => void;
    onCancel: () => void;
  } = $props();

  // The local draft, reset on each open transition (plain latch).
  let nameDraft = $state("");
  let descriptionDraft = $state("");

  function reseed(): void {
    nameDraft = "";
    descriptionDraft = "";
  }

  // Narrower than the server's rule (the gateway refuses empty
  // names, '/' and ':'); a duplicate name is refused server-side.
  const nameError = $derived(
    !/^[A-Za-z0-9_-]+$/.test(nameDraft)
      ? manage_profiles_set_dialog_name_invalid()
      : names.includes(nameDraft.trim())
        ? manage_profiles_set_dialog_name_taken()
        : null,
  );
  const canSubmit = $derived(nameError === null);

  function submit(): void {
    onSave({
      name: nameDraft.trim(),
      description: descriptionDraft.trim(),
    });
  }
</script>

<FormDialog
  {open}
  title={manage_gateway_set_add()}
  size="md"
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
      disabled={busy}
      aria-required="true"
    />
  </label>
  {#if nameError}
    <p class="text-xs text-error-500 dark:text-error-400">
      {nameError}
    </p>
  {/if}

  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {manage_profiles_set_dialog_description_label()}
    </span>
    <input
      class="input text-sm"
      bind:value={descriptionDraft}
      disabled={busy}
    />
  </label>
</FormDialog>
