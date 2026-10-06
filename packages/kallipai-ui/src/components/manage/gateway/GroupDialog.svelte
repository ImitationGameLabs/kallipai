<script lang="ts" module>
  // Platform group create/edit dialog. The create shape takes the
  // name only; the edit shape renames and manages the membership in
  // place (each add/remove lands immediately through the page's
  // store callbacks, so the list always mirrors the server). The
  // member picker tightens the main path to searched accounts; the
  // API-direct surface carries no existence precheck (the member
  // list read-back and the 404 answer are the backstop).
  import type { GroupRow } from "../../../lib/manage/gateway/client.ts";

  export interface GatewayGroupDialogResult {
    readonly name: string;
  }
</script>

<script lang="ts">
  import { Dialog, Portal } from "@skeletonlabs/skeleton-svelte";
  import ReqMark from "../../ReqMark.svelte";
  import { Plus, Search, X } from "@lucide/svelte";
  import {
    common_cancel,
    common_save,
    manage_gateway_group_add_member,
    manage_gateway_group_dialog_create_title,
    manage_gateway_group_dialog_edit_title,
    manage_gateway_group_members,
    manage_gateway_group_member_disabled,
    manage_gateway_group_name_label,
    manage_gateway_group_remove_member,
    manage_gateway_group_search_placeholder,
    manage_gateway_group_search_aria,
  } from "../../../paraglide/messages.js";
  import type { AccountSearchRow } from "../../../lib/manage/gateway/client.ts";
  import { TONAL_ICON_SURF } from "../../../lib/classes.ts";

  let {
    open,
    group,
    busy = false,
    error = null,
    onSave,
    onAddMember,
    onRemoveMember,
    onSearch,
    onCancel,
  }: {
    open: boolean;
    /** The group under edit (null = the create shape). */
    group: GroupRow | null;
    busy?: boolean;
    error?: string | null;
    /** Carries the name; create lands a new group, edit renames. */
    onSave: (result: GatewayGroupDialogResult) => void;
    onAddMember: (account: string) => void;
    onRemoveMember: (account: string) => void;
    /** The account search; resolves the picker's rows. */
    onSearch: (query: string) => Promise<AccountSearchRow[]>;
    onCancel: () => void;
  } = $props();

  let nameDraft = $state("");
  let query = $state("");
  let results = $state<AccountSearchRow[]>([]);
  let searching = $state(false);
  let lastOpen = false;
  $effect(() => {
    if (open && !lastOpen) {
      nameDraft = group?.name ?? "";
      query = "";
      results = [];
    }
    lastOpen = open;
  });

  const canSubmit = $derived(nameDraft.trim().length > 0 && !busy);

  async function runSearch(): Promise<void> {
    const q = query.trim();
    if (q.length === 0 || searching) return;
    searching = true;
    try {
      results = await onSearch(q);
    } catch {
      results = [];
    } finally {
      searching = false;
    }
  }

  function onOpenChange(e: { open: boolean }): void {
    if (!e.open && !busy) onCancel();
  }

  function submit(): void {
    if (!canSubmit) return;
    onSave({ name: nameDraft.trim() });
  }
</script>

<Dialog {open} {onOpenChange}>
  <Portal>
    <Dialog.Backdrop class="fixed inset-0 bg-surface-50-950/60 z-50" />
    <Dialog.Positioner class="fixed inset-0 z-50 grid place-items-center p-4">
      <Dialog.Content
        class="card preset-tonal-surface w-full max-w-xl p-6 flex flex-col gap-4 max-h-[85vh] overflow-y-auto"
      >
        <Dialog.Title class="text-lg font-semibold">
          {group === null
            ? manage_gateway_group_dialog_create_title()
            : manage_gateway_group_dialog_edit_title()}
          {#if group !== null}
            <span class="font-mono opacity-80">{group.group_id}</span>
          {/if}
        </Dialog.Title>

        {#if error}
          <p class="text-sm text-error-500 dark:text-error-400">{error}</p>
        {/if}

        <form
          class="flex flex-col gap-4"
          onsubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <label class="flex flex-col gap-1">
            <span class="text-xs font-medium">
              {manage_gateway_group_name_label()}
              <ReqMark />
            </span>
            <input
              class="input text-sm"
              bind:value={nameDraft}
              aria-required="true"
            />
          </label>

          {#if group !== null}
            <div class="flex flex-col gap-2">
              <span class="text-xs font-medium">
                {manage_gateway_group_members()}
              </span>
              <div class="flex gap-2">
                <input
                  class="input text-sm flex-1"
                  placeholder={manage_gateway_group_search_placeholder()}
                  bind:value={query}
                  onkeydown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      void runSearch();
                    }
                  }}
                />
                <button
                  type="button"
                  class="btn preset-outlined-surface-500 hover:preset-filled-surface-500"
                  aria-label={manage_gateway_group_search_aria()}
                  onclick={runSearch}
                  disabled={busy}
                >
                  <Search class="size-4" />
                </button>
              </div>
              {#if results.length > 0}
                <ul
                  class="divide-y divide-surface-300-600 rounded-base border border-surface-300-600 max-h-40 overflow-y-auto"
                >
                  {#each results as account (account.account_id)}
                    <li
                      class="flex items-center justify-between gap-2 px-3 py-1.5 text-sm"
                    >
                      <span class="min-w-0 truncate">
                        <span class="font-mono">{account.username}</span>
                        {#if account.display_name}
                          <span class="opacity-60">
                            ({account.display_name})
                          </span>
                        {/if}
                        {#if account.disabled}
                          <span class="opacity-40">
                            · {manage_gateway_group_member_disabled()}
                          </span>
                        {/if}
                      </span>
                      <button
                        type="button"
                        class="btn-icon {TONAL_ICON_SURF}"
                        aria-label={manage_gateway_group_add_member()}
                        onclick={() => onAddMember(account.account_id)}
                        disabled={busy}
                      >
                        <Plus class="size-4" />
                      </button>
                    </li>
                  {/each}
                </ul>
              {/if}
              <div class="flex flex-wrap gap-2">
                {#each group.members as member (member)}
                  <span
                    class="badge preset-outlined-surface-500 gap-1 font-mono"
                  >
                    {member}
                    <button
                      type="button"
                      aria-label={manage_gateway_group_remove_member()}
                      class="opacity-60 hover:opacity-100"
                      onclick={() => onRemoveMember(member)}
                      disabled={busy}
                    >
                      <X class="size-3" />
                    </button>
                  </span>
                {/each}
              </div>
            </div>
          {/if}

          <div class="flex gap-2">
            <button
              type="button"
              class="btn flex-1 preset-outlined-surface-500 hover:preset-filled-surface-500"
              onclick={onCancel}
            >
              {common_cancel()}
            </button>
            <button
              type="submit"
              class="btn flex-1 preset-filled-primary-500 text-on-primary-500 transition hover:brightness-110"
              disabled={!canSubmit}
            >
              {common_save()}
            </button>
          </div>
        </form>
      </Dialog.Content>
    </Dialog.Positioner>
  </Portal>
</Dialog>
