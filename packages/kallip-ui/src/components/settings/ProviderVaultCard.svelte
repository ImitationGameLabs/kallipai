<script lang="ts">
  // One vault entry row: identity (name + provider id), the storage-mode
  // badge, a masked key with a copy affordance, inline rename, the mode flip
  // (passkey-gated upstack via `canFlip`), and a two-step delete confirm.
  // Mirrors PasskeyCard: owns its interaction state and surfaces mutation
  // errors inline; mutations run through `onRename` / `onFlip` / `onDelete`
  // and throw on failure. Copy runs through `onCopyKey`, which resolves to
  // the usable plaintext or null when THIS browser cannot open the stored
  // blob (a key sealed on another device).
  import type { ProviderSummary } from "@kallipai/kallip-archeion-client";
  import { ArcheionApiError } from "@kallipai/kallip-archeion-client";
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import { Check, Copy, MoreVertical, Trash } from "@lucide/svelte";
  import { copyText } from "../../lib/clipboard.ts";
  import { TONAL_ICON_SURF } from "../../lib/classes.ts";
  import { getLocale } from "../../paraglide/runtime.js";
  import {
    settings_added_date,
    settings_confirm_remove,
    settings_provider_actions_aria,
    settings_error_unknown,
    settings_provider_badge_encrypted,
    settings_provider_badge_plaintext,
    settings_provider_copy_fail,
    settings_provider_flip_to_encrypted,
    settings_provider_flip_to_plaintext,
    settings_provider_name_duplicate,
    common_cancel,
    common_copy,
    common_rename,
    common_remove,
    common_save,
  } from "../../paraglide/messages.js";

  let {
    entry,
    canFlip = false,
    onRename,
    onFlip,
    onDelete,
    onCopyKey,
  }: {
    entry: ProviderSummary;
    // Whether the mode flip is offered at all: only a session that arrived
    // via passkey shows it (the page passes `store.canFlipKeys()`).
    canFlip?: boolean;
    onRename?: (id: string, name: string) => Promise<void> | void;
    onFlip?: (entry: ProviderSummary) => Promise<void> | void;
    onDelete?: (id: string) => Promise<void> | void;
    onCopyKey?: (entry: ProviderSummary) => Promise<string | null>;
  } = $props();

  // The never-show-the-full-key display: a fixed mask plus the last four
  // characters, which are enough for the user to tell entries apart.
  const keyTail = $derived(entry.key_material.slice(-4));

  function formatDate(ts: string): string {
    return new Date(ts).toLocaleDateString(getLocale(), {
      year: "numeric",
      month: "short",
      day: "numeric",
    });
  }

  // Inline rename (only one field edits at a time); a duplicate name is a
  // 409 the archeion answers with, surfaced as its own qualitative line.
  let editing = $state(false);
  let draft = $state("");
  let renameError = $state<string | null>(null);

  function beginRename() {
    draft = entry.name;
    renameError = null;
    actionError = null;
    editing = true;
  }

  async function saveRename() {
    const name = draft.trim();
    if (!name || name === entry.name) {
      editing = false;
      return;
    }
    try {
      await onRename?.(entry.id, name);
      editing = false;
    } catch (e) {
      console.error("[vault] rename failed:", e);
      // Leave the editor open so the user can retry.
      renameError =
        e instanceof ArcheionApiError && e.status === 409
          ? settings_provider_name_duplicate()
          : settings_error_unknown();
    }
  }

  // Mode flip; server copy arrives through the store's replace path.
  let flipping = $state(false);
  let actionError = $state<string | null>(null);

  async function flip() {
    if (flipping) return;
    flipping = true;
    actionError = null;
    try {
      await onFlip?.(entry);
    } catch (e) {
      console.error("[vault] flip failed:", e);
      actionError = settings_error_unknown();
    } finally {
      flipping = false;
    }
  }

  // Two-step delete confirm (irreversible: hard-deletes the stored key).
  let confirming = $state(false);
  let deleteError = $state<string | null>(null);

  async function confirmDelete() {
    deleteError = null;
    try {
      await onDelete?.(entry.id);
    } catch (e) {
      console.error("[vault] delete failed:", e);
      deleteError = settings_error_unknown();
    }
  }

  // Copy affordance with tri-state feedback: the check flash mirrors
  // CopyButton; failure stays visible until the next attempt, because "this
  // browser cannot open it" is a steady property of the row, not a transient.
  let copyState = $state<"idle" | "ok" | "fail">("idle");
  let copyTimer: ReturnType<typeof setTimeout> | undefined;

  async function copyKey() {
    clearTimeout(copyTimer);
    const text = await onCopyKey?.(entry);
    if (text !== null && text !== undefined) {
      if (await copyText(text)) {
        copyState = "ok";
        copyTimer = setTimeout(() => (copyState = "idle"), 1500);
      } else {
        copyState = "fail";
      }
    } else {
      copyState = "fail";
    }
  }
</script>

<li class="card preset-tonal-surface p-3 space-y-2">
  {#if editing}
    <div class="flex flex-wrap gap-2">
      <input
        class="input input-sm flex-1 min-w-32"
        maxlength={64}
        bind:value={draft}
        disabled={flipping}
        onkeydown={(e) => e.key === "Enter" && saveRename()}
      />
      <button
        class="btn btn-sm preset-outlined-primary-500 hover:preset-filled-primary-500"
        onclick={saveRename}
      >
        {common_save()}
      </button>
      <button
        class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
        onclick={() => (editing = false)}>{common_cancel()}</button
      >
    </div>
    {#if renameError}
      <div class="text-xs text-error-600 dark:text-error-500">
        {renameError}
      </div>
    {/if}
  {:else}
    <div class="flex items-start justify-between gap-2">
      <div class="min-w-0 space-y-0.5">
        <div class="flex items-center gap-2">
          <span class="text-sm font-medium truncate">{entry.name}</span>
          <span
            class="shrink-0 text-[0.65rem] px-1.5 py-0.5 rounded-full bg-primary-500/15 text-primary-600 dark:text-primary-500"
          >
            {entry.mode === "encrypted"
              ? settings_provider_badge_encrypted()
              : settings_provider_badge_plaintext()}
          </span>
        </div>
        <div class="text-xs opacity-60 font-mono truncate">
          {entry.provider}{#if entry.base_url}&nbsp;·&nbsp;{entry.base_url}{/if}
        </div>
        <!-- The mask is presentation policy: the full material is one click
             away (copy), so printing it inline buys nothing and invites
             shoulder-surfing; this mirrors the profiles-page masking. -->
        <div class="flex items-center gap-1 group">
          <span class="text-xs opacity-60 font-mono">••••{keyTail}</span>
          <button
            type="button"
            onclick={copyKey}
            aria-label={common_copy()}
            class="rounded p-1 text-surface-500 dark:text-surface-400 opacity-60 hover:bg-surface-200-800 transition"
          >
            {#if copyState === "ok"}
              <Check class="size-3.5" />
            {:else}
              <Copy class="size-3.5" />
            {/if}
          </button>
        </div>
        <div class="text-xs opacity-60">
          {settings_added_date({ date: formatDate(entry.created_at) })}
        </div>
      </div>
      {#if confirming}
        <div class="flex justify-end gap-2">
          <button
            class="btn btn-sm preset-filled-error-500"
            onclick={confirmDelete}>{settings_confirm_remove()}</button
          >
          <button
            class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
            onclick={() => (confirming = false)}>{common_cancel()}</button
          >
        </div>
      {:else}
        <!-- Kebab actions menu (EnrollmentCodeCard pattern): keeps the card
             to identity + status, with the destructive step still two-step. -->
        <div class="flex justify-end">
          <Menu
            positioning={{ placement: "top-end" }}
            onSelect={(e) => {
              if (e.value === "rename" && onRename) beginRename();
              else if (e.value === "flip" && onFlip) void flip();
              else if (e.value === "remove") {
                deleteError = null;
                confirming = true;
              }
            }}
          >
            <Menu.Trigger
              class="size-8 {TONAL_ICON_SURF}"
              aria-label={settings_provider_actions_aria()}
            >
              <MoreVertical class="size-4" />
            </Menu.Trigger>
            <Portal>
              <Menu.Positioner>
                <Menu.Content
                  class="card preset-tonal-surface p-1 min-w-[8rem]"
                >
                  {#if canFlip}
                    <Menu.Item
                      value="flip"
                      class="px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                    >
                      {flipping ? "… " : ""}{entry.mode === "plaintext"
                        ? settings_provider_flip_to_encrypted()
                        : settings_provider_flip_to_plaintext()}
                    </Menu.Item>
                  {/if}
                  <Menu.Item
                    value="rename"
                    class="px-3 py-2 rounded-base text-sm cursor-pointer hover:preset-filled-surface-500"
                  >
                    {common_rename()}
                  </Menu.Item>
                  <Menu.Item
                    value="remove"
                    class="flex items-center gap-2 px-3 py-2 rounded-base text-sm text-error-500 dark:text-error-400 cursor-pointer hover:preset-filled-error-500"
                  >
                    <Trash class="size-4" />
                    {common_remove()}
                  </Menu.Item>
                </Menu.Content>
              </Menu.Positioner>
            </Portal>
          </Menu>
        </div>
      {/if}
    </div>
    {#if copyState === "fail"}
      <div class="text-xs text-error-600 dark:text-error-500">
        {settings_provider_copy_fail()}
      </div>
    {/if}
    {#if actionError}
      <div class="text-xs text-error-600 dark:text-error-500">
        {actionError}
      </div>
    {/if}
    {#if deleteError}
      <div class="text-xs text-error-600 dark:text-error-500">
        {deleteError}
      </div>
    {/if}
  {/if}
</li>
