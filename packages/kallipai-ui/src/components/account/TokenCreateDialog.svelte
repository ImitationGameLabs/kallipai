<script lang="ts" module>
  // Minting a personal access token: the name, the lifetime and the
  // permission list. The dialog owns the draft and resets it on every open
  // transition, so a cancelled mint leaves nothing behind for the next one.
  // The page keeps the request: this file reports the three values and the
  // shell carries the busy latch, the failure line and the save key.
</script>

<script lang="ts">
  import FormDialog from "../FormDialog.svelte";
  import PermChecklist from "../PermChecklist.svelte";
  import ReqMark from "../ReqMark.svelte";
  import { PAT_DEFAULT_PERMS } from "../../lib/perms.ts";
  import {
    account_tokens_create,
    account_tokens_creating,
    account_tokens_expiry,
    account_tokens_expiry_days,
    account_tokens_name,
    account_tokens_name_placeholder,
    account_tokens_name_required,
    account_tokens_perms,
    account_tokens_perms_required,
  } from "../../paraglide/messages.js";

  let {
    open,
    busy = false,
    error = null,
    onSave,
    onCancel,
  }: {
    open: boolean;
    busy?: boolean;
    error?: string | null;
    onSave: (mint: { name: string; perms: string[]; days: number }) => void;
    onCancel: () => void;
  } = $props();

  const expiryChoices = [30, 90, 365];

  let nameDraft = $state("");
  let daysDraft = $state(90);
  let permsDraft = $state<string[]>([...PAT_DEFAULT_PERMS]);
  // A hint about an empty field follows typing in it, not the open: the
  // dialog opens on the default permission set with no name, and a red line
  // the moment it appears would report a mistake nobody made yet.
  let nameTouched = $state(false);
  let permsTouched = $state(false);

  function reseed(): void {
    nameDraft = "";
    daysDraft = 90;
    permsDraft = [...PAT_DEFAULT_PERMS];
    nameTouched = false;
    permsTouched = false;
  }

  const nameMissing = $derived(nameTouched && nameDraft.trim() === "");
  const permsMissing = $derived(permsTouched && permsDraft.length === 0);
  const canSubmit = $derived(nameDraft.trim() !== "" && permsDraft.length > 0);

  function submit(): void {
    onSave({ name: nameDraft.trim(), perms: permsDraft, days: daysDraft });
  }
</script>

<FormDialog
  {open}
  title={account_tokens_create()}
  size="md"
  {busy}
  {error}
  {canSubmit}
  submitLabel={busy ? account_tokens_creating() : account_tokens_create()}
  onOpen={reseed}
  onSubmit={submit}
  {onCancel}
>
  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {account_tokens_name()}
      <ReqMark />
    </span>
    <input
      class="input text-sm"
      maxlength="64"
      placeholder={account_tokens_name_placeholder()}
      bind:value={nameDraft}
      oninput={() => (nameTouched = true)}
      disabled={busy}
      aria-required="true"
      aria-invalid={nameMissing}
      aria-describedby={nameMissing ? "token-name-hint" : undefined}
    />
    {#if nameMissing}
      <span
        id="token-name-hint"
        class="text-xs text-error-500 dark:text-error-400"
      >
        {account_tokens_name_required()}
      </span>
    {/if}
  </label>

  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">{account_tokens_expiry()}</span>
    <select class="select text-sm" bind:value={daysDraft} disabled={busy}>
      {#each expiryChoices as days (days)}
        <option value={days}
          >{account_tokens_expiry_days({ days: String(days) })}</option
        >
      {/each}
    </select>
  </label>

  <fieldset
    class="space-y-1"
    aria-describedby={permsMissing ? "token-perms-hint" : undefined}
  >
    <legend class="text-xs opacity-60">
      {account_tokens_perms()}
      <ReqMark />
    </legend>
    <PermChecklist
      value={permsDraft}
      onChange={(next) => {
        permsDraft = next;
        permsTouched = true;
      }}
      disabled={busy}
    />
    {#if permsMissing}
      <span
        id="token-perms-hint"
        class="text-xs text-error-500 dark:text-error-400"
      >
        {account_tokens_perms_required()}
      </span>
    {/if}
  </fieldset>
</FormDialog>
