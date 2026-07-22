<script lang="ts">
  // The tagmata dashboard: a centered column listing the owner's tagmata across
  // their lifecycle -- pending tagmas (an enrollment code, not yet connected)
  // first, then enrolled tagmas (a herald connected). One section, one load
  // phase (pending + enrolled are a single agora list now). The "New Tagma"
  // primary action is appended after the list (1 tagma -> 2nd, 2 -> 3rd...); on
  // the first-run empty state it promotes to a centered hero. Prop-driven; the
  // owning store does all fetching + mutations. Per-card mutation errors
  // (rename/revoke) surface inline on the card itself, not here.
  import {
    type EnrollmentCodeCardProps,
    type SectionPhase,
    type TagmaCardProps,
  } from "../../lib/tagmata.svelte.ts";
  import EnrollmentCodeCard from "./EnrollmentCodeCard.svelte";
  import TagmaCard from "./TagmaCard.svelte";

  let {
    pending,
    enrolled,
    phase,
    busy = false,
    onMint,
    onRevoke,
    onCopyCode,
    onRename,
    onOpenChannel,
    copiedCodeId,
  }: {
    pending: EnrollmentCodeCardProps[];
    enrolled: TagmaCardProps[];
    phase: SectionPhase;
    // True while a mint is in flight (disables the New Tagma card).
    busy?: boolean;
    onMint?: () => void;
    // Revoke works for both pending and enrolled (the agora cuts an enrolled
    // herald off on its next request). Awaitable so the enrolled dialog can hold
    // open through the round-trip.
    onRevoke?: (id: string) => Promise<void> | void;
    onCopyCode?: (id: string, secret: string) => void;
    // Rename works for both pending and enrolled. Awaitable: the card holds the
    // inline edit open through the round-trip.
    onRename?: (id: string, label: string) => Promise<void> | void;
    // Open an E2EE channel to an enrolled, online tagma's herald. Awaitable:
    // the card shows a spinner through the key exchange.
    onOpenChannel?: (id: string) => Promise<string> | void;
    // Id of the code whose secret was just copied (drives the "Copied" label).
    copiedCodeId?: string | null;
  } = $props();

  // First-run empty state: nothing to show but the primary action, so promote it
  // to a centered hero. Requires `loaded` so the hero does not flash during the
  // initial fetch.
  const isEmpty = $derived(
    phase === "loaded" && pending.length === 0 && enrolled.length === 0,
  );
</script>

<!--
  Centered column (mirrors the chat / auth pages): a max-width column centered
  in the viewport, with the tagma list stacked inside. The first-run empty state
  centers an enlarged New Tagma card vertically as well.
-->
<div class="h-full overflow-auto">
  <div class="mx-auto w-full max-w-2xl p-4 flex flex-col gap-4 min-h-full">
    {#if isEmpty}
      <!-- First-run empty state: a single centered, enlarged primary CTA. -->
      {#if onMint}
        <div class="flex flex-1 items-center justify-center">
          <button
            type="button"
            class="card preset-filled-primary-500 w-full max-w-md p-8 text-center transition hover:brightness-110 disabled:opacity-60"
            disabled={busy}
            onclick={() => onMint()}
          >
            <div class="text-2xl font-semibold">
              {busy ? "Minting…" : "New Tagma"}
            </div>
            <div class="opacity-80">
              Mint an enrollment code to enroll your first device or identity.
            </div>
          </button>
        </div>
      {/if}
    {:else if phase === "loading"}
      <p class="text-sm opacity-60">Loading...</p>
    {:else if phase === "error"}
      <p class="text-sm text-error-500">Failed to load tagmata.</p>
    {:else}
      <!-- Pending cards first (time-sensitive codes), then enrolled. -->
      <div class="flex flex-col gap-3">
        {#each pending as code (code.id)}
          <EnrollmentCodeCard
            {code}
            onCopy={onCopyCode}
            {onRevoke}
            {onRename}
            copied={copiedCodeId === code.id}
          />
        {/each}
        {#each enrolled as t (t.tagmaId)}
          <TagmaCard tagma={t} {onRename} {onRevoke} {onOpenChannel} />
        {/each}
        {#if onMint}
          <button
            type="button"
            class="card preset-tonal-surface flex items-center justify-center gap-2 py-4 text-sm font-medium transition hover:preset-filled-primary-500 disabled:opacity-60"
            disabled={busy}
            onclick={() => onMint()}
          >
            {busy ? "Minting…" : "+ New Tagma"}
          </button>
        {/if}
      </div>
    {/if}
  </div>
</div>
