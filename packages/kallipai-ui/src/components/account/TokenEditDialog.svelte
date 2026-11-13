<script lang="ts" module>
  // Rescoping one token. The permission list is the only field: the name and
  // the lifetime are settled when the token is minted. The draft starts from
  // every stored string the row carries, unknown ones included, so opening
  // the dialog never drops a grant and saving what the row already fails on
  // stays an error rather than a silent rescope.
</script>

<script lang="ts">
  import type { TokenView } from "@kallipai/kallipai-archeion-client";
  import FormDialog from "../FormDialog.svelte";
  import PermChecklist from "../PermChecklist.svelte";
  import ReqMark from "../ReqMark.svelte";
  import { splitPerms } from "../../lib/perms.ts";
  import {
    account_tokens_edit,
    account_tokens_perms,
    account_tokens_perms_required,
  } from "../../paraglide/messages.js";

  let {
    open,
    token = null,
    busy = false,
    error = null,
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The row being edited; null while the dialog is closed. */
    token?: TokenView | null;
    busy?: boolean;
    error?: string | null;
    onSave: (perms: string[]) => void;
    onCancel: () => void;
  } = $props();

  let permsDraft = $state<string[]>([]);
  let touched = $state(false);

  // The strings this build cannot label still get a checkbox, so the draft
  // shows the whole grant and the save keeps it.
  const unknown = $derived(splitPerms(token?.perms ?? []).unknown);

  function reseed(): void {
    permsDraft = token ? [...token.perms] : [];
    touched = false;
  }

  const permsMissing = $derived(touched && permsDraft.length === 0);
  const canSubmit = $derived(permsDraft.length > 0);

  function submit(): void {
    onSave(permsDraft);
  }
</script>

{#snippet titleExtra()}
  <span class="font-mono opacity-80 break-all">{token?.name ?? ""}</span>
{/snippet}

<FormDialog
  {open}
  title={account_tokens_edit()}
  {titleExtra}
  size="md"
  {busy}
  {error}
  {canSubmit}
  onOpen={reseed}
  onSubmit={submit}
  {onCancel}
>
  <fieldset
    class="space-y-1"
    aria-describedby={permsMissing ? "token-edit-perms-hint" : undefined}
  >
    <legend class="text-xs opacity-60">
      {account_tokens_perms()}
      <ReqMark />
    </legend>
    <PermChecklist
      value={permsDraft}
      onChange={(next) => {
        permsDraft = next;
        touched = true;
      }}
      {unknown}
      disabled={busy}
    />
    {#if permsMissing}
      <span
        id="token-edit-perms-hint"
        class="text-xs text-error-500 dark:text-error-400"
      >
        {account_tokens_perms_required()}
      </span>
    {/if}
  </fieldset>
</FormDialog>
