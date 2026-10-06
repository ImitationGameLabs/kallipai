<script lang="ts" module>
  // Credential dialog (the manage face): the upstream base URL and
  // key land together (the server masks the key on every exit). The
  // URL is validated with the same predicate the server applies, so a
  // relative value is refused before the request leaves; the stored
  // credential can also be removed outright (the page confirms).
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import SecretInput from "../../SecretInput.svelte";
  import ReqMark from "../../ReqMark.svelte";
  import { isAbsoluteUrl } from "../../../lib/gateway/client.ts";
  import {
    common_cancel,
    common_save,
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
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      baseUrlDraft = row?.base_url ?? "";
      apiKeyDraft = "";
      touched = false;
    }
    lastOpen = open;
  });

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

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }

  function submit(): void {
    touched = true;
    if (!canSubmit || busy) return;
    onSave({ baseUrl: baseUrlDraft.trim(), apiKey: apiKeyDraft });
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
          {user_gateway_credential_title({ id: row?.provider_id ?? "" })}
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

          {#if error}
            <p class="text-sm text-error-500" role="alert">{error}</p>
          {/if}

          <div class="flex gap-2">
            <button
              type="button"
              class="btn flex-1 preset-outlined-error-500 hover:preset-filled-error-500"
              onclick={onRemove}
              disabled={busy}
            >
              {user_gateway_credential_remove()}
            </button>
            <button
              type="button"
              class="btn preset-outlined-surface-500 hover:preset-filled-surface-500"
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
