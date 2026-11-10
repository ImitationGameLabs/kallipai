<script lang="ts" module>
  // Group edit dialog: the name and the member accounts. The dialog
  // holds a local draft seeded on each open transition; the page lands
  // it as one create or update. The reserved audience name is refused
  // server-side; the dialog forwards and surfaces the reason.
</script>

<script lang="ts">
  import FormDialog from "../FormDialog.svelte";
  import ReqMark from "../ReqMark.svelte";
  import {
    user_gateway_group_add,
    user_gateway_group_edit_title,
    user_gateway_group_members,
    user_gateway_group_name,
  } from "../../paraglide/messages.js";

  let {
    open,
    row,
    busy = false,
    error = null,
    onSave,
    onCancel,
  }: {
    open: boolean;
    /** The group under edit (null = the create form). */
    row: { group_id: string; name: string; members: string[] } | null;
    busy?: boolean;
    error?: string | null;
    onSave: (result: { name: string; members: string[] }) => void;
    onCancel: () => void;
  } = $props();

  let nameDraft = $state("");
  let membersDraft = $state("");
  function reseed(): void {
    nameDraft = row?.name ?? "";
    membersDraft = row ? row.members.join("\n") : "";
  }

  const canSubmit = $derived(nameDraft.trim() !== "");

  function parseMembers(raw: string): string[] {
    return raw
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line !== "");
  }

  function submit(): void {
    onSave({ name: nameDraft.trim(), members: parseMembers(membersDraft) });
  }
</script>

<FormDialog
  {open}
  title={row === null
    ? user_gateway_group_add()
    : user_gateway_group_edit_title()}
  {busy}
  {error}
  {canSubmit}
  onOpen={reseed}
  onSubmit={submit}
  {onCancel}
>
  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {user_gateway_group_name()}
      <ReqMark />
    </span>
    <input class="input text-sm" bind:value={nameDraft} aria-required="true" />
  </label>
  <label class="flex flex-col gap-1">
    <span class="text-xs font-medium">
      {user_gateway_group_members()}
    </span>
    <textarea
      class="textarea text-sm font-mono"
      rows="5"
      bind:value={membersDraft}></textarea>
  </label>
</FormDialog>
