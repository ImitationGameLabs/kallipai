<script lang="ts">
  // One enrolled tagma: its label, a live online/offline dot, and the enrollment
  // time. The label is editable in place via the kebab menu's Rename action; the
  // kebab also offers Revoke, which opens a confirmation dialog (revoking an
  // enrolled tagma is one-click irreversible and cuts the device off on its next
  // request).
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import {
    Check,
    LoaderCircle,
    MessageSquare,
    MoreVertical,
    Trash,
    X,
  } from "@lucide/svelte";
  import {
    type TagmaCardProps,
    formatDateTime,
    presenceDotClass,
    presenceLabel,
  } from "../../lib/tagmata.svelte.ts";
  import RevokeTagmaDialog from "./RevokeTagmaDialog.svelte";

  let {
    tagma,
    onRename,
    onRevoke,
    onOpenChannel,
  }: {
    tagma: TagmaCardProps;
    // Awaitable: the card holds the edit open through the round-trip.
    onRename?: (id: string, label: string) => Promise<void> | void;
    // Awaitable: the dialog stays open through the round-trip and surfaces a
    // failure inline rather than closing + dropping the error.
    onRevoke?: (id: string) => Promise<void> | void;
    // Open an E2EE channel to this tagma's herald (online + enrolled only).
    // Awaitable: the button shows a spinner through the key exchange and
    // surfaces a failure inline. The handler owns navigation to the chat view.
    onOpenChannel?: (id: string) => Promise<string> | void;
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

  // Open-channel in flight (the key exchange is a round-trip). A failure stays
  // inline so the user sees why the channel did not open.
  let opening = $state(false);
  let openError = $state<string | null>(null);

  async function onOpenChannelClick() {
    if (opening || !onOpenChannel) return;
    opening = true;
    openError = null;
    try {
      await onOpenChannel(tagma.tagmaId);
    } catch (e) {
      openError = e instanceof Error ? e.message : String(e);
    } finally {
      opening = false;
    }
  }

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
          class="size-7 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-primary-500"
          disabled={saving}
          onclick={save}
          aria-label="Save name"
        >
          <Check class="size-4" />
        </button>
        <button
          type="button"
          class="size-7 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-surface-500"
          disabled={saving}
          onclick={cancel}
          aria-label="Cancel rename"
        >
          <X class="size-4" />
        </button>
      </div>
    {:else}
      <h3 class="text-base font-semibold truncate">
        {tagma.label ?? "Unnamed tagma"}
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
    <p>enrolled {formatDateTime(tagma.createdAt)}</p>
    {#if renameError}
      <p class="text-error-500 text-xs">Rename failed: {renameError}</p>
    {/if}
    {#if openError}
      <p class="text-error-500 text-xs">Channel failed: {openError}</p>
    {/if}
  </div>

  {#if onOpenChannel || onRename || onRevoke}
    <!-- Bottom action row. "Open channel" sits bottom-left (online + enrolled
         only); the kebab settings menu sits bottom-right. Hidden (not removed)
         during edit so the row keeps its space. -->
    <div
      class="flex items-center justify-between gap-2"
      class:invisible={editing}
    >
      {#if tagma.presence === "online" && onOpenChannel}
        <button
          type="button"
          class="btn btn-sm preset-tonal-surface hover:preset-filled-primary-500"
          disabled={opening}
          onclick={onOpenChannelClick}
        >
          {#if opening}
            <LoaderCircle class="size-4 animate-spin" />
          {:else}
            <MessageSquare class="size-4" />
          {/if}
          <span>{opening ? "Opening…" : "Open channel"}</span>
        </button>
      {:else}
        <span></span>
      {/if}
      <Menu
        positioning={{ placement: "top-end" }}
        onSelect={(e) => {
          if (e.value === "rename" && onRename) startRename();
          else if (e.value === "revoke" && onRevoke) confirmingRevoke = true;
        }}
      >
        <Menu.Trigger
          class="size-8 grid place-items-center rounded-base preset-tonal-surface hover:preset-filled-surface-500"
          aria-label="Tagma actions"
        >
          <MoreVertical class="size-4" />
        </Menu.Trigger>
        <Portal>
          <Menu.Positioner>
            <Menu.Content class="card preset-tonal-surface p-1 min-w-[8rem]">
              {#if onRename}
                <Menu.Item
                  value="rename"
                  class="px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                >
                  Rename
                </Menu.Item>
              {/if}
              {#if onRevoke}
                <Menu.Item
                  value="revoke"
                  class="flex items-center gap-2 px-3 py-2 rounded-base text-sm text-error-500 cursor-pointer hover:preset-filled-error-500"
                >
                  <Trash class="size-4" />
                  Revoke
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
