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
  import LinkedAccounts from "../components/settings/LinkedAccounts.svelte";
  import EmailManager from "../components/settings/EmailManager.svelte";
  import LightSwitch from "../components/LightSwitch.svelte";
  import LanguageSwitch from "../components/LanguageSwitch.svelte";
  import {
    settings_appearance,
    settings_dark_mode,
    settings_language,
    settings_title,
    settings_heading,
    settings_account,
    settings_connection,
    settings_connected,
    settings_disconnected,
    settings_disconnect,
    settings_reconnect,
    settings_device_added,
    settings_cancelled,
    settings_reauth_failed,
    settings_passkey_duplicate,
    settings_rate_limited,
    settings_error_unknown,
  } from "../paraglide/messages.js";

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

  // Linked OAuth identities + the configured-provider list (for the link
  // affordance) load when the signed-in user resolves. Both guard on a loaded
  // flag (NOT on length): a zero-provider deploy is a valid steady state, and
  // the flag is set on both success and failure so the effect does not refetch.
  $effect(() => {
    if (mode === "online" && agoraSession.user) {
      if (!agoraSession.externalIdentitiesLoaded) {
        agoraSession.refreshExternalIdentities();
      }
      if (!agoraSession.oauthProvidersLoaded) {
        agoraSession.refreshOAuthProviders();
      }
    }
  });

  // Project the wire types into the prop-driven components. The store owns the
  // ceremony + mutations; this page only projects state and forwards callbacks.
  const passkeyCards = $derived(
    agoraSession.passkeys.map(
      (p: PasskeySummary): PasskeyCardProps => ({
        id: p.id,
        label: p.label,
        createdAt: p.created_at,
        lastUsedAt: p.last_used_at,
        discoverable: p.discoverable,
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

  async function onAdd(
    label: string,
    opts: { discoverable?: boolean } = {},
  ): Promise<boolean> {
    adding = true;
    try {
      return (await agoraSession.addPasskey(label, opts)).ok;
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
    if (r.ok) return { tone: "ok", text: settings_device_added() };
    const map: Record<string, string> = {
      cancelled: settings_cancelled(),
      "reauth-required": settings_reauth_failed(),
      "duplicate-credential": settings_passkey_duplicate(),
      "rate-limited": settings_rate_limited(),
      unknown: r.message ?? settings_error_unknown(),
    };
    return { tone: "err", text: map[r.reason] ?? settings_error_unknown() };
  }
</script>

<svelte:head><title>{settings_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div
    class="px-6 pt-[calc(1.5rem+env(safe-area-inset-top))] pb-6 max-w-md space-y-6"
  >
    <h1 class="text-xl font-semibold text-center md:text-left">
      {settings_heading()}
    </h1>

    <section class="space-y-3">
      <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
        {settings_appearance()}
      </h2>
      <div
        class="card preset-tonal-surface p-4 flex items-center justify-between gap-3"
      >
        <div class="text-sm">{settings_dark_mode()}</div>
        <LightSwitch />
      </div>
      <div
        class="card preset-tonal-surface p-4 flex items-center justify-between gap-3"
      >
        <div class="text-sm">{settings_language()}</div>
        <LanguageSwitch />
      </div>
    </section>

    {#if mode === "online"}
      {#if agoraSession.user}
        {@const me = agoraSession.user}
        <section class="space-y-3">
          <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
            {settings_account()}
          </h2>
          <div class="card preset-tonal-surface p-4">
            <!-- display_name is nullable; fall back to the username handle when
                 unset (presentation policy lives here, not the data layer). -->
            <div class="min-w-0">
              <div class="text-sm font-medium truncate">
                {me.display_name ?? me.username}
              </div>
              <div class="text-xs opacity-60 font-mono break-all">
                @{me.username}
              </div>
            </div>
          </div>
        </section>

        <EmailManager />

        <LinkedAccounts />

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
          {settings_connection()}
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
                ? settings_connected()
                : settings_disconnected()}</span
            >
          </div>
          <div class="text-xs opacity-60 font-mono break-all">{offlineUrl}</div>
          <div class="flex flex-wrap gap-2">
            {#if channelsStore.localConnected}
              <button
                class="btn btn-sm preset-tonal-surface"
                onclick={disconnect}>{settings_disconnect()}</button
              >
            {:else}
              <a href="/connect" class="btn btn-sm preset-filled-primary-500"
                >{settings_reconnect()}</a
              >
            {/if}
          </div>
        </div>
      </section>
    {/if}
  </div>
</div>
