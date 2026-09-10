<script lang="ts" module>
  // The unified "New tagma" dialog: two parallel option blocks, mirroring the
  // OAuth provider chooser's one-tap-per-path affordance. Path A (one-click,
  // cloud) mints + spawns + self-enrolls; its "Advanced" disclosure expands
  // the manual launch fields. Path B (local agent) mints an enrollment code
  // the user hands to a self-run tagma, with a QR of the code plaintext.
  //
  // Prop-driven like CreateRoomDialog: the dialog never touches a store. It
  // drafts the fields and hands them to the owning page, which performs the
  // store calls and closes the dialog on success (failures come back down as
  // the `error` prop; `busy` disables every action while any call is out).
  export interface AdvancedSpawnFields {
    slug: string;
    workspace: string;
    archeionUrl: string;
    enrollmentCode: string;
    lescheUrl: string;
    instanceToken: string;
    llmProvider: string;
    llmModel: string;
    llmApiKey: string;
  }
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import FormError from "../FormError.svelte";
  import {
    isLocked,
    type PushCandidate,
  } from "../../lib/instances/credentialPush.ts";
  import { copyText } from "../../lib/clipboard.ts";
  import {
    common_cancel,
    common_copy,
    common_copied,
    common_create,
    manage_instances_advanced_toggle,
    manage_instances_create_creating,
    manage_instances_create_provider_label,
    manage_instances_create_provider_locked_hint,
    manage_instances_create_provider_model_label,
    manage_instances_create_provider_model_placeholder,
    manage_instances_create_provider_none,
    manage_instances_create_workspace_label,
    manage_instances_create_workspace_placeholder,
    manage_instances_dialog_desc,
    manage_instances_dialog_title,
    manage_instances_mint_action,
    manage_instances_mint_done_hint,
    tagmata_minting,
    manage_instances_method_cloud_desc,
    manage_instances_method_cloud_title,
    manage_instances_method_local_desc,
    manage_instances_method_local_title,
    manage_instances_spawn_archeion_label,
    manage_instances_spawn_enrollment_label,
    manage_instances_spawn_lesche_label,
    manage_instances_spawn_llm_api_key_label,
    manage_instances_spawn_llm_model_label,
    manage_instances_spawn_llm_provider_label,
    manage_instances_spawn_slug_label,
    manage_instances_spawn_slug_placeholder,
    manage_instances_spawn_submit,
    manage_instances_spawn_token_label,
    manage_instances_spawn_workspace_label,
  } from "../../paraglide/messages.js";

  let {
    open,
    busy = false,
    error = null,
    // "designated-user" capability gate: hides path A entirely when the
    // instances service cannot spawn (the dialog then opens on path B).
    canSpawn = false,
    // Vault rows offered as one-click credential sources (page-supplied;
    // empty = the select degrades to a skip-only dropdown).
    providers = [],
    sessionViaPasskey = false,
    onOneClick,
    onSpawn,
    onMint,
    onCancel,
  }: {
    open: boolean;
    busy?: boolean;
    error?: string | null;
    canSpawn?: boolean;
    providers?: PushCandidate[];
    sessionViaPasskey?: boolean;
    onOneClick: (opts: {
      workspace: string;
      providerId?: string | null;
      /** Required when providerId is set (the bound set's model). */
      model?: string;
    }) => Promise<void> | void;
    onSpawn: (fields: AdvancedSpawnFields) => Promise<void> | void;
    onMint: () => Promise<{ id: string; code: string } | null>;
    onCancel: () => void;
  } = $props();

  // Drafts. Reset on every open transition (the lastOpen latch pattern from
  // CreateRoomDialog) so a prior draft, error, or minted code never lingers.
  let method = $state<"cloud" | "local">("cloud");
  let workspace = $state("");
  let providerId = $state<string | null>(null);
  let pushModel = $state("");
  let advanced = $state(false);
  let fields = $state<AdvancedSpawnFields>({
    slug: "",
    workspace: "",
    archeionUrl: "",
    enrollmentCode: "",
    lescheUrl: "",
    instanceToken: "",
    llmProvider: "",
    llmModel: "",
    llmApiKey: "",
  });
  let minted = $state<{ code: string } | null>(null);
  let copied = $state(false);
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      method = canSpawn ? "cloud" : "local";
      workspace = "";
      providerId = null;
      pushModel = "";
      advanced = false;
      fields = {
        slug: "",
        workspace: "",
        archeionUrl: "",
        enrollmentCode: "",
        lescheUrl: "",
        instanceToken: "",
        llmProvider: "",
        llmModel: "",
        llmApiKey: "",
      };
      minted = null;
      copied = false;
    }
    lastOpen = open;
  });

  const pickedProvider = $derived(
    providerId ? (providers.find((p) => p.id === providerId) ?? null) : null,
  );
  // Fail closed: a credential pick without a model name cannot launch.
  const canSubmit = $derived(
    !busy &&
      !(providerId && !pushModel.trim()) &&
      (method === "local" ||
        (advanced
          ? fields.slug.trim().length > 0 && fields.workspace.trim().length > 0
          : workspace.trim().length > 0)),
  );

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }

  async function copy(): Promise<void> {
    if (!minted) return;
    if (await copyText(minted.code)) {
      copied = true;
      setTimeout(() => (copied = false), 2000);
    }
    // On copy failure the minted code stays selectable as a manual fallback.
  }

  function submit(): void {
    if (!canSubmit) return;
    if (method === "local") {
      void onMint().then((m) => {
        if (m) minted = { code: m.code };
      });
      return;
    }
    if (advanced) {
      void onSpawn({
        slug: fields.slug.trim(),
        workspace: fields.workspace.trim(),
        archeionUrl: fields.archeionUrl,
        enrollmentCode: fields.enrollmentCode,
        lescheUrl: fields.lescheUrl,
        instanceToken: fields.instanceToken,
        llmProvider: fields.llmProvider,
        llmModel: fields.llmModel,
        llmApiKey: fields.llmApiKey,
      });
      return;
    }
    void onOneClick({
      workspace: workspace.trim(),
      providerId,
      model: pushModel.trim(),
    });
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-lg p-6 flex flex-col gap-4"
      >
        <Dialog.Title class="text-lg font-semibold">
          {manage_instances_dialog_title()}
        </Dialog.Title>
        <Dialog.Description class="sr-only">
          {manage_instances_dialog_desc()}
        </Dialog.Description>

        <!-- Parallel option blocks (the OAuth-chooser affordance): one tap
             picks the path; the selected block's controls render below. -->
        <div class="grid sm:grid-cols-2 gap-3">
          {#if canSpawn}
            <button
              type="button"
              aria-pressed={method === "cloud"}
              class="text-left rounded-xl border p-4 space-y-1 transition {method ===
              'cloud'
                ? 'border-primary-500 bg-primary-500/10'
                : 'border-surface-200-800 hover:border-surface-300-700'}"
              onclick={() => (method = "cloud")}
            >
              <span class="block text-sm font-medium">
                {manage_instances_method_cloud_title()}
              </span>
              <span class="block text-xs opacity-70">
                {manage_instances_method_cloud_desc()}
              </span>
            </button>
          {/if}
          <button
            type="button"
            aria-pressed={method === "local"}
            class="text-left rounded-xl border p-4 space-y-1 transition {method ===
            'local'
              ? 'border-primary-500 bg-primary-500/10'
              : 'border-surface-200-800 hover:border-surface-300-700'}"
            onclick={() => (method = "local")}
          >
            <span class="block text-sm font-medium">
              {manage_instances_method_local_title()}
            </span>
            <span class="block text-xs opacity-70">
              {manage_instances_method_local_desc()}
            </span>
          </button>
        </div>

        {#if method === "cloud"}
          <form
            class="flex flex-col gap-4"
            onsubmit={(e) => {
              e.preventDefault();
              submit();
            }}
          >
            {#if !advanced}
              <label class="flex flex-col gap-1">
                <span class="text-sm font-medium">
                  {manage_instances_create_workspace_label()}
                  <span class="text-error-500 dark:text-error-400">*</span>
                </span>
                <input
                  class="input text-sm"
                  placeholder={manage_instances_create_workspace_placeholder()}
                  bind:value={workspace}
                  disabled={busy}
                  required
                />
              </label>
              <label class="flex flex-col gap-1">
                <span class="text-sm font-medium">
                  {manage_instances_create_provider_label()}
                </span>
                <select
                  class="input text-sm"
                  bind:value={providerId}
                  disabled={busy}
                >
                  <option value={null}>
                    {manage_instances_create_provider_none()}
                  </option>
                  {#each providers as p (p.id)}
                    <option
                      value={p.id}
                      disabled={isLocked(p, sessionViaPasskey)}
                    >
                      {p.name}
                    </option>
                  {/each}
                </select>
                {#if providers.some((p) => isLocked(p, sessionViaPasskey))}
                  <span class="text-xs opacity-70">
                    {manage_instances_create_provider_locked_hint()}
                  </span>
                {/if}
                {#if pickedProvider}
                  <span class="text-sm font-medium">
                    {manage_instances_create_provider_model_label()}
                    <span class="text-error-500 dark:text-error-400">*</span>
                  </span>
                  <input
                    class="input text-sm"
                    placeholder={manage_instances_create_provider_model_placeholder()}
                    bind:value={pushModel}
                    disabled={busy}
                    required
                  />
                {/if}
              </label>
            {/if}

            <button
              type="button"
              class="self-start text-xs underline underline-offset-2 opacity-70"
              onclick={() => (advanced = !advanced)}
            >
              {manage_instances_advanced_toggle()}
            </button>

            {#if advanced}
              <!-- The manual launch fields: slug + workspace are required;
                   the optional relay/LLM fields become the daemon env
                   allowlist on the page side. -->
              <div class="grid gap-4 sm:grid-cols-2">
                <label class="flex flex-col gap-1">
                  <span class="text-sm font-medium">
                    {manage_instances_spawn_slug_label()}
                    <span class="text-error-500 dark:text-error-400">*</span>
                  </span>
                  <input
                    class="input text-sm"
                    placeholder={manage_instances_spawn_slug_placeholder()}
                    bind:value={fields.slug}
                    disabled={busy}
                    required
                  />
                </label>
                <label class="flex flex-col gap-1">
                  <span class="text-sm font-medium">
                    {manage_instances_spawn_workspace_label()}
                    <span class="text-error-500 dark:text-error-400">*</span>
                  </span>
                  <input
                    class="input text-sm"
                    placeholder={manage_instances_create_workspace_placeholder()}
                    bind:value={fields.workspace}
                    disabled={busy}
                    required
                  />
                </label>
                <label class="flex flex-col gap-1 sm:col-span-2">
                  <span class="text-sm opacity-70">
                    {manage_instances_spawn_archeion_label()}
                  </span>
                  <input
                    class="input text-sm"
                    bind:value={fields.archeionUrl}
                  />
                </label>
                <label class="flex flex-col gap-1 sm:col-span-2">
                  <span class="text-sm opacity-70">
                    {manage_instances_spawn_enrollment_label()}
                  </span>
                  <input
                    class="input text-sm"
                    bind:value={fields.enrollmentCode}
                  />
                </label>
                <label class="flex flex-col gap-1 sm:col-span-2">
                  <span class="text-sm opacity-70">
                    {manage_instances_spawn_lesche_label()}
                  </span>
                  <input class="input text-sm" bind:value={fields.lescheUrl} />
                </label>
                <label class="flex flex-col gap-1 sm:col-span-2">
                  <span class="text-sm opacity-70">
                    {manage_instances_spawn_token_label()}
                  </span>
                  <input
                    class="input text-sm"
                    type="password"
                    bind:value={fields.instanceToken}
                  />
                </label>
                <label class="flex flex-col gap-1">
                  <span class="text-sm opacity-70">
                    {manage_instances_spawn_llm_provider_label()}
                  </span>
                  <input
                    class="input text-sm"
                    bind:value={fields.llmProvider}
                  />
                </label>
                <label class="flex flex-col gap-1">
                  <span class="text-sm opacity-70">
                    {manage_instances_spawn_llm_model_label()}
                  </span>
                  <input class="input text-sm" bind:value={fields.llmModel} />
                </label>
                <label class="flex flex-col gap-1 sm:col-span-2">
                  <span class="text-sm opacity-70">
                    {manage_instances_spawn_llm_api_key_label()}
                  </span>
                  <input
                    class="input text-sm"
                    type="password"
                    bind:value={fields.llmApiKey}
                  />
                </label>
              </div>
            {/if}

            {#if error}
              <FormError message={error} />
            {/if}

            <div class="flex gap-2">
              <button
                type="button"
                class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"
                disabled={busy}
                onclick={onCancel}
              >
                {common_cancel()}
              </button>
              <button
                type="submit"
                class="btn flex-1 preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110"
                disabled={!canSubmit}
              >
                {#if busy}
                  {manage_instances_create_creating()}
                {:else if advanced}
                  {manage_instances_spawn_submit()}
                {:else}
                  {common_create()}
                {/if}
              </button>
            </div>
          </form>
        {:else}
          <div class="flex flex-col gap-4">
            {#if minted}
              <!-- The plaintext shows once, here only; after a refresh the
                   pending row on the page carries the masked form. -->
              <div class="flex flex-col items-center gap-3">
                <code
                  class="text-xs font-mono break-all select-all bg-surface-100-900 border border-surface-200-800 rounded-lg px-3 py-2"
                  >{minted.code}</code
                >
                <button
                  type="button"
                  class="btn btn-sm preset-tonal-surface"
                  onclick={() => void copy()}
                >
                  {copied ? common_copied() : common_copy()}
                </button>
                <p class="text-xs opacity-60">
                  {manage_instances_mint_done_hint()}
                </p>
              </div>
            {:else}
              {#if error}
                <FormError message={error} />
              {/if}
              <button
                type="button"
                class="btn preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110"
                disabled={busy}
                onclick={() => submit()}
              >
                {busy ? tagmata_minting() : manage_instances_mint_action()}
              </button>
            {/if}
            <button
              type="button"
              class="self-start text-xs underline underline-offset-2 opacity-70"
              disabled={busy}
              onclick={onCancel}
            >
              {common_cancel()}
            </button>
          </div>
        {/if}
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
