<script lang="ts">
  // The passkeys section: the live-passkey list with its load phase, plus the
  // unified add-device entry (another device via a pairing code, or another
  // passkey on this browser). Prop-driven and portable (mirrors the tagmata
  // dashboard split); the owning page maps the agora store into these props and
  // wires the mutations. Per-card rename/revoke errors surface on the card.
  import type {
    PairingCodeView,
    PasskeyAddHint,
    PasskeyCardProps,
    PasskeyPhase,
  } from "../../lib/passkeys.svelte.ts";
  import AddDevice from "./AddDevice.svelte";
  import PasskeyCard from "./PasskeyCard.svelte";

  let {
    passkeys,
    phase,
    error = null,
    adding = false,
    addHint = null,
    onAdd,
    onRename,
    onRevoke,
    pairingCode = null,
    pairingError = null,
    minting = false,
    onMint,
    onClear,
  }: {
    passkeys: PasskeyCardProps[];
    phase: PasskeyPhase;
    // Section-level fetch error (a list load failure does not blank `user`).
    error?: string | null;
    // Local add-passkey path.
    adding?: boolean;
    addHint?: PasskeyAddHint | null;
    onAdd?: (label: string) => Promise<boolean> | boolean | void;
    onRename?: (id: string, label: string) => Promise<void> | void;
    onRevoke?: (id: string) => Promise<void> | void;
    // Cross-device pairing-code path.
    pairingCode?: PairingCodeView | null;
    pairingError?: string | null;
    minting?: boolean;
    onMint?: () => void | Promise<void>;
    onClear?: () => void;
  } = $props();
</script>

<section class="space-y-3">
  <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
    Passkeys
  </h2>

  {#if error}
    <div class="text-xs text-error-600">{error}</div>
  {/if}

  {#if phase === "loading"}
    <p class="text-sm opacity-60">Loading...</p>
  {:else}
    <ul class="space-y-2">
      {#each passkeys as pk (pk.id)}
        <PasskeyCard passkey={pk} {onRename} {onRevoke} />
      {/each}
    </ul>
  {/if}

  <AddDevice
    {adding}
    {addHint}
    {onAdd}
    {pairingCode}
    {pairingError}
    {minting}
    {onMint}
    {onClear}
  />
</section>
