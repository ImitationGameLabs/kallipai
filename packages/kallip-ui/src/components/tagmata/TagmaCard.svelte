<script lang="ts">
  // One tagma card on the unified /tagmata page. Two kinds share this shape,
  // split by what backs the card -- an enrolled identity joined with a local
  // process ("hosted", badged: workspace + process rows show, stop/open
  // appear) vs an identity alone ("self-run": no process rows or actions;
  // revoke is the one real management move). A process-only card renders the
  // hosted shape minus the identity rows. Rename stays an inline edit; Revoke
  // keeps its confirmation dialog (one-click irreversible, cuts the device
  // off on its next request).
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import { timezoneSetting } from "../../lib/time/stamp.svelte.ts";
  import {
    Check,
    ExternalLink,
    MoreVertical,
    Play,
    Square,
    Trash,
    X,
  } from "@lucide/svelte";
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
  import { tagmaDetailsSectionPath } from "../../lib/shell/routes.ts";
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
    manage_instances_running,
    manage_instances_stopped,
    manage_instances_start,
    manage_instances_stop,
    nav_chat,
    tagmata_badge_connected,
    tagmata_badge_hosted,
    tagmata_badge_connected_title,
    tagmata_badge_hosted_title,
    tagmata_identity_pending,
    tagma_revoke,
    tagma_enrolled_at,
    tagma_rename_failed,
    auth_couldnt_reach,
  } from "../../paraglide/messages.js";
  import { ArcheionApiError } from "@kallipai/kallip-archeion-client";

  let {
    tagma = undefined,
    process = undefined,
    onRename,
    onStop,
    onStart,
    onRevoke,
  }: {
    // Present when an enrolled identity backs this card; absent for a
    // process-only card (a local process whose slug carries no identity).
    tagma?: TagmaCardProps;
    // Awaitable: the card holds the edit open through the round-trip.
    onRename?: (id: string, label: string) => Promise<void> | void;
    // The host-side process. Its presence is what makes the card hosted:
    // the workspace/process rows render and stop/open become available.
    process?: {
      slug: string;
      workspace: string;
      running: boolean;
      port?: number;
    };
    // Opens the page-level stop confirmation (the card never stops
    // directly -- destructive actions get the dialog treatment).
    onStop?: (slug: string) => void;
    // Relaunches a stopped or dead instance from its persisted tree;
    // absent for self-run cards (nothing host-side to restart).
    onStart?: (slug: string) => Promise<void> | void;
    // Awaitable: the dialog stays open through the round-trip and surfaces a
    // failure inline rather than closing + dropping the error.
    onRevoke?: (id: string) => Promise<void> | void;
  } = $props();

  // The two card kinds differ by what backs them, not by how they were
  // created: a process in hand (hosted) vs an enrolled identity alone
  // (self-run). A spawned tagma whose process later disappears simply
  // falls back to the self-run shape, so the card never offers a control
  // it does not have.
  const hosted = $derived(process !== undefined);

  const name = $derived(
    tagma ? (tagma.label ?? tagma_profile_unnamed()) : (process?.slug ?? ""),
  );
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

  /** Typed archeion failures keep their server copy; transport failures are
   * qualitative, details to the console. */
  function msgOf(e: unknown): string {
    if (e instanceof ArcheionApiError) return e.message;
    console.error("[tagma] rename/revoke failed:", e);
    return auth_couldnt_reach();
  }
  // Manage-rooms dialog (lazy: only opened on demand).
  let roomsOpen = $state(false);

  async function confirmRevoke() {
    if (revoking || !onRevoke || !tagma) return;
    revoking = true;
    revokeError = null;
    try {
      await onRevoke(tagma.tagmaId);
      confirmingRevoke = false;
    } catch (e) {
      revokeError = msgOf(e);
    } finally {
      revoking = false;
    }
  }

  function startRename() {
    if (!tagma) return;
    draft = tagma.label ?? "";
    renameError = null;
    editing = true;
    queueMicrotask(() => inputEl?.focus());
  }

  async function save() {
    if (saving || !onRename || !tagma) return;
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
      renameError = msgOf(e);
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
      <h3
        class="text-base font-semibold truncate flex items-center gap-2 min-w-0"
      >
        <span class="truncate">{name}</span>
        <span
          class="badge preset-filled-surface-500 text-xs shrink-0 align-middle"
          title={hosted
            ? tagmata_badge_hosted_title()
            : tagmata_badge_connected_title()}
          >{hosted ? tagmata_badge_hosted() : tagmata_badge_connected()}</span
        >
      </h3>
      {#if tagma}
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
      {:else}
        <!-- A process-only card has no identity-side liveness signal; the
             neutral surface dot keeps the header column aligned. -->
        <span
          class="size-2 rounded-full shrink-0 bg-surface-300-700"
          aria-hidden="true"
        ></span>
      {/if}
    {/if}
  </div>

  <div class="flex flex-col gap-1 text-sm opacity-80">
    {#if tagma}
      <p class="font-mono text-sm break-all">{tagma.tagmaId}</p>
      <p>
        {tagma_enrolled_at({
          date: formatDateTime(tagma.createdAt, timezoneSetting.value),
        })}
      </p>
      {#if tagma.status}
        <p class="text-xs opacity-70">{formatTagmaStatusLine(tagma.status)}</p>
      {/if}
    {/if}
    {#if process}
      {#if process && !tagma}
        <!-- A process with no enrolled identity: name the absence so the
           missing id/enrolled-at rows read as a state, not an error. -->
        <p class="text-xs opacity-70">{tagmata_identity_pending()}</p>
      {/if}
      <p class="font-mono text-xs opacity-70 break-all">{process.workspace}</p>
      <p
        class="text-xs {process.running
          ? 'text-success-500 dark:text-success-400'
          : 'opacity-70'}"
      >
        {process.running
          ? manage_instances_running()
          : manage_instances_stopped()}
      </p>
    {/if}
    {#if renameError}
      <p class="text-error-500 dark:text-error-400 text-xs">
        {tagma_rename_failed({ error: renameError })}
      </p>
    {/if}
  </div>

  {#if tagma || process}
    <!-- Bottom action row: the kebab menu, right-aligned. Identity actions
         (manage / rooms / rename / revoke) need a tagma; the process actions
         (open / stop) need a live process -- each renders only when its
         precondition holds, so a self-run card never shows a control we do
         not have. Hidden (not removed) during edit so the row keeps its
         space. -->
    <div class="flex items-center justify-end gap-2" class:invisible={editing}>
      <Menu
        positioning={{ placement: "top-end" }}
        onSelect={(e) => {
          if (e.value === "open" && process?.port) {
            navigate(`/connect?tagmaUrl=http://127.0.0.1:${process.port}`);
          } else if (e.value === "stop" && process && onStop) {
            onStop(process.slug);
          } else if (
            e.value === "start" &&
            process &&
            !process.running &&
            onStart
          ) {
            void onStart?.(process.slug);
          } else if (e.value === "manage" && tagma) {
            navigate(tagmaDetailsSectionPath(tagma.tagmaId, "overview"));
          } else if (e.value === "rooms" && tagma) roomsOpen = true;
          else if (e.value === "rename" && tagma && onRename) startRename();
          else if (e.value === "revoke" && tagma && onRevoke)
            confirmingRevoke = true;
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
              {#if tagma}
                <Menu.Item
                  value="manage"
                  class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                >
                  <Settings class="size-4" />
                  {tagma_menu_manage()}
                </Menu.Item>
              {/if}
              {#if tagma}
                <Menu.Item
                  value="rooms"
                  class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                >
                  <DoorOpen class="size-4" />
                  {tagma_menu_manage_rooms()}
                </Menu.Item>
              {/if}
              {#if process?.port}
                <Menu.Item
                  value="open"
                  class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                >
                  <ExternalLink class="size-4" />
                  {nav_chat()}
                </Menu.Item>
              {/if}
              {#if process && !process.running && onStart}
                <Menu.Item
                  value="start"
                  class="flex items-center gap-2 px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                >
                  <Play class="size-4" />
                  {manage_instances_start()}
                </Menu.Item>
              {/if}

              {#if tagma && onRename}
                <Menu.Item
                  value="rename"
                  class="px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                >
                  {common_rename()}
                </Menu.Item>
              {/if}
              <!-- Danger zone: destructive actions cluster at the tail,
                   past a separator (the AccountMenu pattern), so a
                   misaimed click never lands on Stop/Revoke from the
                   safe-zone scroll. -->
              {#if (process?.running && onStop) || (tagma && onRevoke)}
                <Menu.Separator class="my-1 border-surface-200-800" />
              {/if}
              {#if process?.running && onStop}
                <Menu.Item
                  value="stop"
                  class="flex items-center gap-2 px-3 py-2 rounded-base text-sm text-error-500 dark:text-error-400 cursor-pointer hover:preset-filled-error-500"
                >
                  <Square class="size-4" />
                  {manage_instances_stop()}
                </Menu.Item>
              {/if}
              {#if tagma && onRevoke}
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
  tagmaLabel={tagma?.label ?? null}
  busy={revoking}
  error={revokeError}
  onConfirm={confirmRevoke}
  onCancel={() => {
    confirmingRevoke = false;
    revokeError = null;
  }}
/>

{#if tagma}
  <ManageTagmaRoomsDialog
    open={roomsOpen}
    tagmaId={tagma.tagmaId}
    tagmaLabel={tagma.label}
    onCancel={() => {
      roomsOpen = false;
    }}
  />
{/if}
