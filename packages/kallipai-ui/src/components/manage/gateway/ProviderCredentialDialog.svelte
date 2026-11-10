<script lang="ts" module>
  // Credential dialog (the manage face): the upstream base URL and
  // key land together (the server masks the key on every exit). The
  // URL is validated with the same predicate the server applies, so a
  // relative value is refused before the request leaves; the stored
  // credential can also be removed outright (the page confirms).
</script>

<script lang="ts">
  import FormDialog from "../../FormDialog.svelte";
  import SecretInput from "../../SecretInput.svelte";
  import ReqMark from "../../ReqMark.svelte";
  import { isAbsoluteUrl } from "../../../lib/gateway/client.ts";
  import {
    manage_gateway_provider_base,
    manage_gateway_provider_key,
    user_gateway_credential_key_blank,
    user_gateway_credential_remove,
    user_gateway_credential_title,
    user_gateway_credential_url_invalid,
  } from "../../../paraglide/messages.js";

  let {
    open,
    row,
    busy = false,
    error = null,
    onSave,
    onRemove,
    onCancel,
  }: {
    open: boolean;
    /** The provider whose credential is managed. */
    row: { provider_id: string; base_url: string | null } | null;
    busy?: boolean;
    error?: string | null;
    onSave: (result: { baseUrl: string; apiKey: string }) => void;
    onRemove: () => void;
    onCancel: () => void;
  } = $props();

  let baseUrlDraft = $state("");
  let apiKeyDraft = $state("");
  let touched = $state(false);

  function reseed(): void {
    baseUrlDraft = row?.base_url ?? "";
    apiKeyDraft = "";
    touched = false;
  }

  // The same predicate the server applies to upstream_base_url: only
  // values `new URL()` parses are accepted. The blank-key check is the
  // server's too (a blank key would store a secret matching nothing).
  const urlInvalid = $derived(
    touched &&
      baseUrlDraft.trim() !== "" &&
      !isAbsoluteUrl(baseUrlDraft.trim()),
  );
  const keyBlank = $derived(touched && apiKeyDraft === "");
  const canSubmit = $derived(
    isAbsoluteUrl(baseUrlDraft.trim()) && apiKeyDraft !== "",
  );

  // The shell's gate keeps this function off a form that cannot submit, so
  // `touched` is set by typing: a blank key shows its hint once the field has
  // been visited, not on a press the disabled save key never delivers. Two
  // dialogs reveal a hint this way, this one and the user credential dialog.
  function submit(): void {
    onSave({ baseUrl: baseUrlDraft.trim(), apiKey: apiKeyDraft });
  }
</script>

<FormDialog
  {open}
  title={user_gateway_credential_title({ id: row?.provider_id ?? "" })}
  {busy}
  {error}
  {canSubmit}
  onOpen={reseed}
  onSubmit={submit}
  {onCancel}
  destructiveAction={{
    label: user_gateway_credential_remove(),
    onAction: onRemove,
  }}
>
  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {manage_gateway_provider_base()}
      <ReqMark />
    </span>
    <input
      class="input text-sm"
      bind:value={baseUrlDraft}
      oninput={() => (touched = true)}
      placeholder="https://"
      aria-required="true"
    />
    {#if urlInvalid}
      <span class="text-xs text-error-500">
        {user_gateway_credential_url_invalid()}
      </span>
    {/if}
  </label>
  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {manage_gateway_provider_key()}
      <ReqMark />
    </span>
    <SecretInput
      bind:value={apiKeyDraft}
      oninput={() => (touched = true)}
      aria-required="true"
    />
    {#if keyBlank}
      <span class="text-xs text-error-500">
        {user_gateway_credential_key_blank()}
      </span>
    {/if}
  </label>
</FormDialog>
