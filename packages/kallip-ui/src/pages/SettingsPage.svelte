<script lang="ts">
  import { agoraSession } from "../lib/session/agora.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { configStore } from "../lib/config/config.svelte";
  import { modeOf } from "../lib/config/mode.ts";
  import type {
    AddPasskeyResult,
    PasskeySummary,
  } from "@kallipai/kallip-agora-client";
  import type {
    PasskeyAddHint,
    PasskeyCardProps,
    PasskeyPhase,
  } from "../lib/passkeys.svelte.ts";
  import PasskeyManager from "../components/settings/PasskeyManager.svelte";

  // Settings is now info-only: account actions (logout, mode switch) live in
  // the sidebar AccountMenu. Online shows the account (identity lives in
  // agora); offline shows the tagma connection (no identity). Offline
  // Disconnect/Reconnect stays here -- it is tagma session management, not an
  // account/mode action.
  const mode = $derived(modeOf(configStore.value));
  const offlineUrl = $derived(configStore.value?.offline?.tagmaUrl ?? "");

  // Offline: drop the tagma session without abandoning offline mode.
  function disconnect() {
    channelsStore.detachLocal();
  }

  // -- passkeys (online only) ----------------------------------------------
  // Loaded once when the signed-in user resolves. The store mirrors the tagmata
  // error discipline: a fetch failure lands in `passkeysError` without blanking
  // `user`; rename/revoke throw and are surfaced per-card.
  $effect(() => {
    if (
      mode === "online" &&
      agoraSession.user &&
      !agoraSession.passkeysLoaded
    ) {
      agoraSession.refreshPasskeys();
    }
  });

  // Wire the wire types into the prop-driven components. The store owns the
  // ceremony + mutations; this page only projects state and forwards callbacks.
  const passkeyCards = $derived(
    agoraSession.passkeys.map(
      (p: PasskeySummary): PasskeyCardProps => ({
        id: p.id,
        label: p.label,
        createdAt: p.created_at,
        lastUsedAt: p.last_used_at,
      }),
    ),
  );

  const passkeyPhase = $derived<PasskeyPhase>(
    agoraSession.passkeysError
      ? "error"
      : agoraSession.passkeysLoaded
        ? "loaded"
        : "loading",
  );

  const passkeyAddHint = $derived(addHintFor(agoraSession.lastAddPasskey));

  let adding = $state(false);

  async function onAdd(label: string): Promise<boolean> {
    adding = true;
    try {
      return (await agoraSession.addPasskey(label)).ok;
    } finally {
      adding = false;
    }
  }

  // Cross-device pairing: mint a short-lived code shown on this device for a
  // new device to redeem. `minting` gates the button; `onClear` drops an expired
  // code from the store (the countdown fires it).
  let minting = $state(false);

  async function onMint() {
    if (minting) return;
    minting = true;
    try {
      await agoraSession.mintPairingCode();
    } finally {
      minting = false;
    }
  }

  // Project the add-device ceremony result into a renderable hint. The result
  // type is a client type; the hint is a pure view shape, so the mapping lives
  // here (the components stay free of the client import).
  function addHintFor(r: AddPasskeyResult | null): PasskeyAddHint | null {
    if (!r) return null;
    if (r.ok) return { tone: "ok", text: "Device added." };
    const map: Record<string, string> = {
      cancelled: "Cancelled.",
      "reauth-required": "Re-authentication failed; try again.",
      "duplicate-credential": "That device is already registered.",
      "rate-limited": "Too many attempts. Try again later.",
      unknown: r.message ?? "Something went wrong.",
    };
    return { tone: "err", text: map[r.reason] ?? "Something went wrong." };
  }
</script>

<svelte:head><title>KallipAI · settings</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="p-6 max-w-md space-y-6">
    <h1 class="text-xl font-semibold">Settings</h1>

    {#if mode === "online"}
      {#if agoraSession.user}
        {@const me = agoraSession.user}
        <section class="space-y-3">
          <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
            Account
          </h2>
          <div class="card preset-tonal-surface p-4">
            <!-- display_name is nullable; fall back to the username handle when
                 unset (presentation policy lives here, not the data layer). -->
            <div class="min-w-0">
              <div class="text-sm font-medium truncate">
                {me.display_name ?? me.username}
              </div>
              <div class="text-xs opacity-60 font-mono break-all">
                {me.email}
              </div>
            </div>
          </div>
        </section>

        <PasskeyManager
          passkeys={passkeyCards}
          phase={passkeyPhase}
          error={agoraSession.passkeysError}
          addHint={passkeyAddHint}
          {adding}
          {onAdd}
          onRename={(id, label) => agoraSession.renamePasskey(id, label)}
          onRevoke={(id) => agoraSession.revokePasskey(id)}
          pairingCode={agoraSession.pairingCode}
          pairingError={agoraSession.pairingError}
          {minting}
          {onMint}
          onClear={() => (agoraSession.pairingCode = null)}
        />
      {/if}
    {:else}
      <section class="space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          Connection
        </h2>
        <div class="card preset-tonal-surface p-4 space-y-3">
          <div class="flex items-center gap-2 text-sm">
            <span
              class="size-2 rounded-full {channelsStore.localConnected
                ? 'bg-success-500'
                : 'bg-error-500'}"
              aria-hidden="true"
            ></span>
            <span class="font-medium"
              >{channelsStore.localConnected
                ? "Connected"
                : "Disconnected"}</span
            >
          </div>
          <div class="text-xs opacity-60 font-mono break-all">{offlineUrl}</div>
          <div class="flex flex-wrap gap-2">
            {#if channelsStore.localConnected}
              <button
                class="btn btn-sm preset-tonal-surface"
                onclick={disconnect}>Disconnect</button
              >
            {:else}
              <a href="/connect" class="btn btn-sm preset-filled-primary-500"
                >Reconnect</a
              >
            {/if}
          </div>
        </div>
      </section>
    {/if}
  </div>
</div>
