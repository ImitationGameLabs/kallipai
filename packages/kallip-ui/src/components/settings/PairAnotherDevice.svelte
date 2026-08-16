<script lang="ts">
  // The cross-device "add" form: mint a short-lived pairing code and show it as
  // a typeable code + QR with a live countdown. A new device types it or scans
  // it to enroll its own passkey onto this account (see PairPage). Form-only and
  // prop-driven (the page maps agora state into these props); the chooser +
  // trigger live in `AddDevice`. User-facing copy says "Add a device"; the
  // internal name keeps "pair" as the mechanism (the issued credential is a
  // pairing code, like Bluetooth pairing).
  import {
    pairSecondsRemaining,
    type PairingCodeView,
  } from "../../lib/passkeys.svelte.ts";
  import {
    settings_pair_qr_alt,
    settings_pair_show,
    settings_pair_expires,
    settings_copy_code,
    settings_pair_intro,
    settings_generate_pair_code,
    common_copied,
    common_generating,
  } from "../../paraglide/messages.js";
  import QRCode from "qrcode";

  let {
    view = null,
    error = null,
    minting = false,
    onMint,
    onClear,
  }: {
    // The currently minted code view, or null until one is minted.
    view?: PairingCodeView | null;
    // Mint-step error (e.g. step-up re-auth failed).
    error?: string | null;
    // True while the mint request is in flight.
    minting?: boolean;
    onMint?: () => void | Promise<void>;
    // Called when the displayed code expires (the owner drops it from state).
    onClear?: () => void;
  } = $props();

  let copied = $state(false);
  // The QR encodes a deep link `${origin}/pair?code=...` so a scanner opens the
  // pair page with the code pre-filled. Recomputed each time a code is minted.
  let qrDataUrl = $state<string | null>(null);
  // Ticks every second while a code is shown, to drive the countdown.
  let now = $state(Date.now());

  const remaining = $derived(
    view ? pairSecondsRemaining(view.expiresAt, now) : 0,
  );

  // Drive the countdown + drop the code when it expires.
  $effect(() => {
    if (!view) return;
    now = Date.now();
    const id = setInterval(() => {
      now = Date.now();
      if (pairSecondsRemaining(view.expiresAt, now) <= 0) onClear?.();
    }, 1000);
    return () => clearInterval(id);
  });

  // Render the QR whenever a fresh code appears.
  $effect(() => {
    if (!view) {
      qrDataUrl = null;
      return;
    }
    const url = `${window.location.origin}/pair?code=${encodeURIComponent(view.code)}`;
    QRCode.toDataURL(url, { width: 240, margin: 1 })
      .then((d: string) => (qrDataUrl = d))
      .catch(() => (qrDataUrl = null));
  });

  async function copy() {
    if (!view) return;
    try {
      await navigator.clipboard.writeText(view.code);
      copied = true;
      setTimeout(() => (copied = false), 2000);
    } catch {
      // Clipboard may be unavailable; ignore.
    }
  }
</script>

{#if view}
  <div class="flex flex-col items-center gap-3">
    {#if qrDataUrl}
      <img
        src={qrDataUrl}
        alt={settings_pair_qr_alt()}
        class="w-48 h-48 rounded"
      />
    {/if}
    <div class="text-center">
      <div class="text-xs opacity-60">{settings_pair_show()}</div>
      <div class="text-2xl font-mono tracking-widest font-semibold mt-1">
        {view.code}
      </div>
      <div class="text-xs opacity-60 mt-1">
        {settings_pair_expires({ seconds: remaining })}
      </div>
    </div>
    <button class="btn btn-sm preset-tonal-surface" onclick={copy}>
      {copied ? common_copied() : settings_copy_code()}
    </button>
  </div>
{:else}
  <div class="space-y-2">
    <p class="text-sm opacity-70">
      {settings_pair_intro()}
    </p>
    <button
      class="btn btn-sm preset-filled-primary-500"
      disabled={minting}
      onclick={onMint}
    >
      {minting ? common_generating() : settings_generate_pair_code()}
    </button>
    {#if error}
      <div class="text-xs text-error-600 dark:text-error-500">{error}</div>
    {/if}
  </div>
{/if}
