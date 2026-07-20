<script lang="ts">
  // One pending tagma (an enrollment code, not yet first-connected). `code` is
  // whatever the agora returned for this row: the full plaintext straight from
  // the mint response (only while `copyable` -- the user's one chance to copy
  // it), or the agora's masked `sk-enroll-abc***xyz` from the list endpoint.
  // The label is editable in place via the kebab menu's Rename action, mirroring
  // the enrolled TagmaCard.
  import { onMount } from "svelte";
  import { Menu, Portal } from "@skeletonlabs/skeleton-svelte";
  import { Check, MoreVertical, Trash, X } from "@lucide/svelte";
  import {
    type EnrollmentCodeCardProps,
    formatRemaining,
    isExpired,
  } from "../../lib/tagmata.svelte.ts";

  let {
    code,
    onCopy,
    onRevoke,
    onRename,
    copied = false,
  }: {
    code: EnrollmentCodeCardProps;
    onCopy?: (id: string, secret: string) => void;
    // Awaitable: a failed revoke surfaces inline (the store throws rather than
    // blanking the whole dashboard).
    onRevoke?: (id: string) => Promise<void> | void;
    // Awaitable: the card holds the edit open through the round-trip.
    onRename?: (id: string, label: string) => Promise<void> | void;
    // Parent-driven "copied" highlight so the clipboard write + state stay in the
    // owning store, not this presentational card.
    copied?: boolean;
  } = $props();

  const expired = $derived(isExpired(code.expiresAt));

  // Live "now" ticking once a minute so the remaining-time countdown stays
  // fresh without a re-fetch. Minute granularity matches `formatRemaining`.
  let now = $state(Date.now());
  onMount(() => {
    const id = setInterval(() => {
      now = Date.now();
    }, 60_000);
    return () => clearInterval(id);
  });
  const remainingMs = $derived(new Date(code.expiresAt).getTime() - now);

  // Inline-edit state (mirrors TagmaCard). `saving` holds the input open until
  // the awaited rename resolves so there is no stale-label flash; a failure
  // keeps the input open with `renameError` shown. `suppressBlur` lets Escape
  // cancel without the subsequent blur re-triggering save.
  let editing = $state(false);
  let draft = $state("");
  let saving = $state(false);
  let renameError = $state<string | null>(null);
  let revokeError = $state<string | null>(null);
  let inputEl: HTMLInputElement | undefined = $state();
  let suppressBlur = false;

  // Revoke is fire-and-forget from the kebab; a failure surfaces inline (the
  // store throws rather than blanking the dashboard).
  async function requestRevoke() {
    if (!onRevoke) return;
    revokeError = null;
    try {
      await onRevoke(code.id);
    } catch (e) {
      revokeError = e instanceof Error ? e.message : String(e);
    }
  }

  function startRename() {
    draft = code.label ?? "";
    renameError = null;
    editing = true;
    queueMicrotask(() => inputEl?.focus());
  }

  async function save() {
    if (saving || !onRename) return;
    const trimmed = draft.trim();
    if ((code.label ?? "") === trimmed) {
      editing = false;
      renameError = null;
      return;
    }
    saving = true;
    renameError = null;
    try {
      await onRename(code.id, trimmed);
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
  A tagma in the pending state. Mirrors TagmaCard's layout: a title row (label
  + status badge), a body (the code + the remaining-time line), and a footer
  (Copy + kebab). Custom padding (not Skeleton's `card-header/body/footer`,
  which sit too close to the border) so content and buttons have real breathing
  room; `overflow-hidden` + `min-w-0` keep the code inside the card's filled box.
-->
<div
  class="card preset-tonal-surface card-hover overflow-hidden flex flex-col gap-4 p-5"
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
        {code.label ?? "Unnamed tagma"}
      </h3>
      {#if expired}
        <span class="badge variant-filled-warning text-xs shrink-0"
          >expired</span
        >
      {:else}
        <span class="badge variant-filled-surface text-xs shrink-0"
          >pending</span
        >
      {/if}
    {/if}
  </div>

  <code
    class="block w-full min-w-0 font-mono text-sm break-all px-3 py-2 rounded preset-tonal"
    title={code.copyable ? code.code : "Secret shown only at mint time"}
  >
    {code.code}
  </code>

  <div class="flex flex-col gap-1 text-sm opacity-80">
    <p class="truncate" title={code.expiresAt}>
      {remainingMs > 0
        ? `expires in ${formatRemaining(remainingMs)}`
        : "expired"}
    </p>
    {#if renameError}
      <p class="text-error-500 text-xs">Rename failed: {renameError}</p>
    {/if}
    {#if revokeError}
      <p class="text-error-500 text-xs">Revoke failed: {revokeError}</p>
    {/if}
  </div>

  <div class="flex items-center justify-between gap-2">
    <div class="flex items-center gap-2">
      {#if code.copyable && onCopy}
        <button
          type="button"
          class="btn min-w-24 preset-outlined-primary-500 hover:preset-filled-primary-500"
          onclick={() => onCopy(code.id, code.code)}
        >
          {copied ? "Copied" : "Copy"}
        </button>
      {/if}
    </div>
    {#if onRename || onRevoke}
      <!-- Kebab "settings" menu, bottom-right. Hosts Rename (neutral) and Revoke
           (destructive: error tone + trash icon). Kept mounted (toggled
           `invisible`, not removed) during edit so its reserved space keeps the
           card a constant size. -->
      <div class="flex justify-end" class:invisible={editing}>
        <Menu
          positioning={{ placement: "top-end" }}
          onSelect={(e) => {
            if (e.value === "rename" && onRename) startRename();
            else if (e.value === "revoke" && onRevoke) void requestRevoke();
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
</div>
