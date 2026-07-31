<script lang="ts">
  // One passkey row: identity (label + creation date), inline rename, and a
  // two-step revoke confirm (irreversible -- hard-deletes the credential).
  // Owns its own interaction state (editing / confirming) and surfaces mutation
  // errors inline; the owning store does the actual work via `onRename` /
  // `onRevoke` and throws on failure (the store does not blank the list).
  import {
    formatPasskeyDate,
    type PasskeyCardProps,
  } from "../../lib/passkeys.svelte.ts";

  let {
    passkey,
    onRename,
    onRevoke,
  }: {
    passkey: PasskeyCardProps;
    onRename?: (id: string, label: string) => Promise<void> | void;
    onRevoke?: (id: string) => Promise<void> | void;
  } = $props();

  // Inline rename (only one field edits at a time). `draft` is (re)seeded in
  // `beginRename`, so the initial value is unused.
  let editing = $state(false);
  let draft = $state("");
  let renameError: string | null = $state(null);

  function beginRename() {
    draft = passkey.label;
    renameError = null;
    editing = true;
  }

  async function saveRename() {
    const label = draft.trim();
    try {
      await onRename?.(passkey.id, label);
      editing = false;
    } catch (e) {
      // Leave the editor open so the user can retry.
      renameError = e instanceof Error ? e.message : String(e);
    }
  }

  // Two-step revoke confirm (irreversible: hard-deletes the credential).
  let confirming = $state(false);
  let revokeError: string | null = $state(null);

  async function confirmRevoke() {
    revokeError = null;
    try {
      await onRevoke?.(passkey.id);
    } catch (e) {
      revokeError = e instanceof Error ? e.message : String(e);
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
        onkeydown={(e) => e.key === "Enter" && saveRename()}
      />
      <button class="btn btn-sm preset-tonal-surface" onclick={saveRename}>
        Save
      </button>
      <button
        class="btn btn-sm preset-tonal-surface"
        onclick={() => (editing = false)}>Cancel</button
      >
    </div>
    {#if renameError}
      <div class="text-xs text-error-600">{renameError}</div>
    {/if}
  {:else}
    <div class="flex items-center justify-between gap-2">
      <div class="min-w-0">
        <div class="text-sm font-medium truncate">
          {passkey.label || "Unnamed device"}
        </div>
        <div class="text-xs opacity-60">
          Added {formatPasskeyDate(passkey.createdAt)}
          {#if Date.parse(passkey.lastUsedAt) > Date.parse(passkey.createdAt)}
            <span class="opacity-70">· Last used {formatPasskeyDate(passkey.lastUsedAt)}</span>
          {/if}
        </div>
      </div>
      <div class="flex gap-2">
        <button class="btn btn-sm preset-tonal-surface" onclick={beginRename}
          >Rename</button
        >
        {#if confirming}
          <button
            class="btn btn-sm preset-filled-error-500"
            onclick={confirmRevoke}>Confirm remove</button
          >
          <button
            class="btn btn-sm preset-tonal-surface"
            onclick={() => (confirming = false)}>Cancel</button
          >
        {:else}
          <button
            class="btn btn-sm preset-tonal-surface"
            onclick={() => {
              revokeError = null;
              confirming = true;
            }}>Remove</button
          >
        {/if}
      </div>
    </div>
    {#if revokeError}
      <div class="text-xs text-error-600">{revokeError}</div>
    {/if}
  {/if}
</li>
