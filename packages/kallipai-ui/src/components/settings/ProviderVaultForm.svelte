<script lang="ts">
  // The "add key" dialog. Prop-driven like AddPasskey: the owner passes the
  // submit callback (which throws on failure) and `canFlip`, which decides
  // whether the encrypt option is offered -- checked by default when this
  // session arrived via passkey (the device vault key exists / will exist),
  // forced off otherwise. The checkbox stays visible-but-disabled without
  // canFlip so an OAuth-only session still learns the feature exists; the
  // "!" badge hover explains why (native title tooltips are pointer-only,
  // touch screens get no hover).
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import type { ProviderKeyMode } from "@kallipai/kallipai-archeion-client";
  import { ArcheionApiError } from "@kallipai/kallipai-archeion-client";
  import { MODEL_PROVIDER_FAMILIES } from "../../lib/providerFamilies.ts";
  import InfoBadge from "../InfoBadge.svelte";
  import SecretInput from "../SecretInput.svelte";
  import {
    settings_provider_new_title,
    settings_provider_name_label,
    settings_provider_name_placeholder,
    settings_provider_family_label,
    settings_provider_base_url_label,
    settings_provider_key_label,
    settings_provider_encrypt_label,
    settings_provider_encrypt_lock_hint,
    settings_provider_plaintext_hint,
    settings_provider_encrypted_hint,
    settings_provider_intro,
    settings_provider_name_duplicate,
    settings_error_unknown,
    common_adding,
    common_cancel,
    common_create,
  } from "../../paraglide/messages.js";

  let {
    open = false,
    // Whether encrypted storage is offered; also picks the default.
    canFlip = false,
    onCreate,
    onClosed,
  }: {
    open?: boolean;
    canFlip?: boolean;
    // Commit the form fields (the store seals encrypted keys before send);
    // resolve true on success so the form resets, false/undefined lets the
    // user retry without retyping.
    onCreate?: (req: {
      name: string;
      provider: string;
      base_url: string | null;
      key_material: string;
      mode: ProviderKeyMode;
    }) => Promise<boolean> | boolean | void;
    onClosed?: () => void;
  } = $props();

  let name = $state("");
  let selectedFamily = $state<string>(MODEL_PROVIDER_FAMILIES[0]);
  let baseUrl = $state("");
  let key = $state("");
  let encrypt = $state(true);
  let error = $state<string | null>(null);
  let busy = $state(false);

  function begin() {
    name = "";
    selectedFamily = MODEL_PROVIDER_FAMILIES[0];
    baseUrl = "";
    key = "";
    encrypt = canFlip;
    error = null;
  }

  begin();

  // Reset drafts on each open transition (lastOpen latch, like
  // ProviderDialog): a fresh dialog never shows the previous attempt.
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) begin();
    lastOpen = open;
  });

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open) onClosed?.();
  }

  async function submit() {
    const trimmedName = name.trim();
    if (!trimmedName || !key || busy) return;
    busy = true;
    try {
      const ok =
        (await onCreate?.({
          name: trimmedName,
          provider: selectedFamily,
          base_url: baseUrl.trim() || null,
          key_material: key,
          mode: encrypt ? "encrypted" : "plaintext",
        })) ?? false;
      if (ok) {
        begin();
        onClosed?.();
      }
    } catch (e) {
      console.error("[vault] create failed:", e);
      error =
        e instanceof ArcheionApiError && e.status === 409
          ? settings_provider_name_duplicate()
          : settings_error_unknown();
    } finally {
      busy = false;
    }
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
          {settings_provider_new_title()}
        </Dialog.Title>
        <Dialog.Description class="sr-only">
          {settings_provider_intro()}
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
              {settings_provider_name_label()}
              <span class="text-error-500 dark:text-error-400">*</span>
            </span>
            <input
              class="input text-sm"
              placeholder={settings_provider_name_placeholder()}
              maxlength={64}
              bind:value={name}
              disabled={busy}
            />
          </label>

          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium">
              {settings_provider_family_label()}
            </span>
            <select
              class="select text-sm"
              bind:value={selectedFamily}
              disabled={busy}
            >
              {#each MODEL_PROVIDER_FAMILIES as f (f)}
                <option value={f}>{f}</option>
              {/each}
            </select>
          </label>

          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium">
              {settings_provider_base_url_label()}
            </span>
            <input
              class="input text-sm font-mono"
              type="url"
              maxlength={256}
              bind:value={baseUrl}
              disabled={busy}
            />
          </label>

          <!-- Key input above; its encrypt option sits right below as a
               sibling row -- never a checkbox nested inside the key label
               (click routing). -->
          <label class="flex flex-col gap-1">
            <span class="text-sm font-medium">
              {settings_provider_key_label()}
              <span class="text-error-500 dark:text-error-400">*</span>
            </span>
            <SecretInput bind:value={key} disabled={busy} />
          </label>
          <label class="flex items-center gap-2 text-sm select-none">
            <input
              type="checkbox"
              bind:checked={encrypt}
              disabled={busy || !canFlip}
            />
            <span>{settings_provider_encrypt_label()}</span>
            {#if !canFlip}
              <InfoBadge text={settings_provider_encrypt_lock_hint()} />
            {/if}
          </label>
          {#if encrypt}
            <p class="text-xs opacity-60">
              {settings_provider_encrypted_hint()}
            </p>
          {:else}
            <p class="text-xs opacity-60">
              {settings_provider_plaintext_hint()}
            </p>
          {/if}

          {#if error}
            <div class="text-xs text-error-600 dark:text-error-500">
              {error}
            </div>
          {/if}

          <div class="flex gap-2">
            <button
              type="button"
              class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"
              onclick={() => onClosed?.()}
            >
              {common_cancel()}
            </button>
            <button
              type="submit"
              class="btn flex-1 preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110"
              disabled={!name.trim() || !key || busy}
            >
              {busy ? common_adding() : common_create()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
