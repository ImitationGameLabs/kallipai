<script lang="ts">
  // One enrolled tagma: its label, a live online/offline dot, and the enrollment
  // time. The label is editable in place via the kebab menu's Rename action; the
  // kebab also offers Revoke, which opens a confirmation dialog (revoking an
  // enrolled tagma is one-click irreversible and cuts the device off on its next
  // request).
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import { Check, MoreVertical, Trash, X } from "@lucide/svelte";
  import {
    type TagmaCardProps,
    formatDateTime,
    formatTagmaStatusLine,
    presenceDotClass,
    presenceLabel,
  } from "../../lib/tagmata.svelte.ts";
  import { DoorOpen, Settings } from "@lucide/svelte";
  import ManageTagmaRoomsDialog from "./ManageTagmaRoomsDialog.svelte";
  import { navigate } from "../../lib/shell/port.ts";
  import { TONAL_ICON_PRIM, TONAL_ICON_SURF } from "../../lib/classes.ts";
  import RevokeTagmaDialog from "./RevokeTagmaDialog.svelte";
  import {
    common_rename,
    tagma_profile_unnamed,
    tagma_save_name_aria,
    tagma_cancel_rename_aria,
    tagma_actions_aria,
    tagma_menu_manage,
    tagma_menu_manage_rooms,
    tagma_revoke,
    tagma_enrolled_at,
    tagma_rename_failed,
  } from "../../paraglide/messages.js";

  let {
    tagma,
    onRename,
    onRevoke,
  }: {
    tagma: TagmaCardProps;
    // Awaitable: the card holds the edit open through the round-trip.
    onRename?: (id: string, label: string) => Promise<void> | void;
    // Awaitable: the dialog stays open through the round-trip and surfaces a
    // failure inline rather than closing + dropping the error.
    onRevoke?: (id: string) => Promise<void> | void;
  } = $props();

  // Inline-edit state. `saving` holds the input open until the awaited rename
  // resolves so there is no stale-label flash; a failure keeps the input open
  // with `renameError` shown. `suppressBlur` lets Escape cancel without the
  // subsequent blur re-triggering save.
  let editing = $state(false);
  let draft = $state("");
  let saving = $state(false);
  let renameError = $state<string | null>(null);
  let inputEl: HTMLInputElement | undefined = $state();
  let suppressBlur = false;

  // Revoke confirmation. The irreversible, immediately-effective action gets a
  // second-chance dialog the pending-code revoke does not. The dialog stays open
  // (with a busy + error line) through the awaited revoke, closing only on
  // success so a failure is surfaced, not dropped.
  let confirmingRevoke = $state(false);
  let revoking = $state(false);
  let revokeError = $state<string | null>(null);

  // Manage-rooms dialog (lazy: only opened on demand).
  let roomsOpen = $state(false);

  async function confirmRevoke() {
    if (revoking || !onRevoke) return;
    revoking = true;
    revokeError = null;
    try {
      await onRevoke(tagma.tagmaId);
      confirmingRevoke = false;
    } catch (e) {
      revokeError = e instanceof Error ? e.message : String(e);
    } finally {
      revoking = false;
    }
  }

  function startRename() {
    draft = tagma.label ?? "";
    renameError = null;
    editing = true;
    queueMicrotask(() => inputEl?.focus());
  }

  async function save() {
    if (saving || !onRename) return;
    const trimmed = draft.trim();
    if ((tagma.label ?? "") === trimmed) {
      editing = false;
      renameError = null;
      return;
    }
    saving = true;
    renameError = null;
    try {
      await onRename(tagma.tagmaId, trimmed);
      editing = false;
    } catch (e) {
      renameError = e instanceof Error ? e.message : String(e);
      queueMicrotask(() => inputEl?.focus());
    } finally {
      saving = false;
    }
  }

  function cancel() {
    editing = false;
    renameError = null;
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      void save();
    } else if (e.key === "Escape") {
      e.preventDefault();
      suppressBlur = true;
      cancel();
    }
  }

  function onBlur() {
    if (suppressBlur) {
      suppressBlur = false;
      return;
    }
    void save();
  }
</script>

<!--
  Mirrors the EnrollmentCodeCard layout: custom padding (not Skeleton's tight
  `card-header/body/footer`). The label is the title (falling back to "Unnamed
  tagma" -- never the raw id); the id lives in the body for reference. Rename is
  an inline edit triggered from the bottom-right kebab menu.
-->
<div
  class="card preset-tonal-surface transition hover:brightness-95 overflow-hidden flex flex-col gap-4 p-5"
>
  <div class="flex items-center justify-between gap-2">
    {#if editing}
      <input
        bind:this={inputEl}
        bind:value={draft}
        type="text"
        maxlength={64}
        disabled={saving}
        onkeydown={onKeydown}
        onblur={onBlur}
        class="input input-sm flex-1 min-w-0"
      />
      <div class="flex items-center gap-1 shrink-0">
        <button
          type="button"
          class="size-7 {TONAL_ICON_PRIM}"
          disabled={saving}
          onclick={save}
          aria-label={tagma_save_name_aria()}
        >
          <Check class="size-4" />
        </button>
        <button
          type="button"
          class="size-7 {TONAL_ICON_SURF}"
          disabled={saving}
          onclick={cancel}
          aria-label={tagma_cancel_rename_aria()}
        >
          <X class="size-4" />
        </button>
      </div>
    {:else}
      <h3 class="text-base font-semibold truncate">
        {tagma.label ?? tagma_profile_unnamed()}
      </h3>
      <span
        class="flex items-center gap-1.5 text-sm opacity-80 shrink-0"
        title={presenceLabel(tagma.presence)}
      >
        <span
          class="size-2 rounded-full {presenceDotClass(tagma.presence)}"
          aria-hidden="true"
        ></span>
        {presenceLabel(tagma.presence)}
      </span>
    {/if}
  </div>

  <div class="flex flex-col gap-1 text-sm opacity-80">
    <p class="font-mono text-sm break-all">{tagma.tagmaId}</p>
    <p>{tagma_enrolled_at({ date: formatDateTime(tagma.createdAt) })}</p>
    {#if tagma.status}
      <p class="text-xs opacity-70">{formatTagmaStatusLine(tagma.status)}</p>
    {/if}
    {#if renameError}
      <p class="text-error-500 dark:text-error-400 text-xs">
        {tagma_rename_failed({ error: renameError })}
      </p>
    {/if}
  </div>

  {#if onRename || onRevoke}
    <!-- Bottom action row: the kebab settings menu (rename / revoke),
         right-aligned. Channels are opened automatically (see channelsStore
         auto-connect), so there is no per-card open action. Hidden (not
         removed) during edit so the row keeps its space. -->
    <div class="flex items-center justify-end gap-2" class:invisible={editing}>
      <Menu
        positioning={{ placement: "top-end" }}
        onSelect={(e) => {
          if (e.value === "manage") {
            navigate(`/chat/t/${tagma.tagmaId}/manage/overview`);
          } else if (e.value === "rooms") roomsOpen = true;
          else if (e.value === "rename" && onRename) startRename();
          else if (e.value === "revoke" && onRevoke) confirmingRevoke = true;
        }}
      >
        <Menu.Trigger
          class="size-8 {TONAL_ICON_SURF}"
          aria-label={tagma_actions_aria()}
        >
          <MoreVertical class="size-4" />
        </Menu.Trigger>
        <Portal>
          <Menu.Positioner>
            <Menu.Content class="card preset-tonal-surface p-1 min-w-[8rem]">
              <Menu.Item
                value="manage"
                class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
              >
                <Settings class="size-4" />
                {tagma_menu_manage()}
              </Menu.Item>
              <Menu.Item
                value="rooms"
                class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
              >
                <DoorOpen class="size-4" />
                {tagma_menu_manage_rooms()}
              </Menu.Item>
              {#if onRename}
                <Menu.Item
                  value="rename"
                  class="px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                >
                  {common_rename()}
                </Menu.Item>
              {/if}
              {#if onRevoke}
                <Menu.Item
                  value="revoke"
                  class="flex items-center gap-2 px-3 py-2 rounded-base text-sm text-error-500 dark:text-error-400 cursor-pointer hover:preset-filled-error-500"
                >
                  <Trash class="size-4" />
                  {tagma_revoke()}
                </Menu.Item>
              {/if}
            </Menu.Content>
          </Menu.Positioner>
        </Portal>
      </Menu>
    </div>
  {/if}
</div>

<RevokeTagmaDialog
  open={confirmingRevoke}
  tagmaLabel={tagma.label}
  busy={revoking}
  error={revokeError}
  onConfirm={confirmRevoke}
  onCancel={() => {
    confirmingRevoke = false;
    revokeError = null;
  }}
/>

<ManageTagmaRoomsDialog
  open={roomsOpen}
  tagmaId={tagma.tagmaId}
  tagmaLabel={tagma.label}
  onCancel={() => {
    roomsOpen = false;
  }}
/>
