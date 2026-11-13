<script lang="ts">
  // The /account page: who you are on this product. The identity card,
  // passkeys with the add-device entry, email addresses, linked providers and
  // the way into the access-token page. Preferences (appearance,
  // notifications, timezone, connection) live on /settings. The desktop
  // sidebar keeps its dropdown; this page is what the bottom bar's trailing
  // cell opens, and the desktop dropdown gains an entry for it.
  // The store owns the ceremonies and mutations; this page projects state and
  // forwards callbacks, so the passkey components stay client-import free.
  import { KeyRound, LogOut, Settings } from "@lucide/svelte";
  import HubRow from "../../components/HubRow.svelte";
  import EmailManager from "../../components/account/EmailManager.svelte";
  import LinkedAccounts from "../../components/account/LinkedAccounts.svelte";
  import PasskeyManager from "../../components/account/PasskeyManager.svelte";
  import { archeionSession } from "../../lib/session/archeion.svelte";
  import { shellMode } from "../../lib/shell/port.ts";
  import { logout } from "../../lib/session/account-actions.ts";
  import type {
    AddPasskeyResult,
    PasskeySummary,
  } from "@kallipai/kallipai-archeion-client";
  import type {
    PasskeyAddHint,
    PasskeyCardProps,
    PasskeyPhase,
  } from "../../lib/passkeys.svelte.ts";
  import {
    account_cancelled,
    account_device_added,
    account_logout,
    account_menu,
    account_passkey_duplicate,
    account_rate_limited,
    account_reauth_failed,
    account_tokens,
    error_unknown,
    settings_heading,
  } from "../../paraglide/messages.js";

  // Branch on mode, not on `user` (the AccountMenu invariant): offline must
  // never act on a stale archeion session, so the identity block and the mode
  // actions follow the mode.
  const mode = $derived(shellMode());

  // -- passkeys (online only) ----------------------------------------------
  // Loaded once when the signed-in user resolves. The store mirrors the tagmata
  // error discipline: a fetch failure lands in `passkeysError` without blanking
  // `user`; rename/revoke throw and are surfaced per-card.
  $effect(() => {
    if (
      mode === "online" &&
      archeionSession.user &&
      !archeionSession.passkeysLoaded
    ) {
      archeionSession.refreshPasskeys();
    }
  });

  // Linked OAuth identities + the configured-provider list (for the link
  // affordance) load when the signed-in user resolves. Both guard on a loaded
  // flag (NOT on length): a zero-provider deploy is a valid steady state, and
  // the flag is set on both success and failure so the effect does not refetch.
  $effect(() => {
    if (mode === "online" && archeionSession.user) {
      if (!archeionSession.externalIdentitiesLoaded) {
        archeionSession.refreshExternalIdentities();
      }
      if (!archeionSession.oauthProvidersLoaded) {
        archeionSession.refreshOAuthProviders();
      }
    }
  });

  // Project the wire types into the prop-driven components.
  const passkeyCards = $derived(
    archeionSession.passkeys.map(
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
    archeionSession.passkeysError
      ? "error"
      : archeionSession.passkeysLoaded
        ? "loaded"
        : "loading",
  );

  const passkeyAddHint = $derived(addHintFor(archeionSession.lastAddPasskey));

  let adding = $state(false);

  async function onAdd(
    label: string,
    opts: { discoverable?: boolean } = {},
  ): Promise<boolean> {
    adding = true;
    try {
      return (await archeionSession.addPasskey(label, opts)).ok;
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
      await archeionSession.mintPairingCode();
    } finally {
      minting = false;
    }
  }

  // Project the add-device ceremony result into a renderable hint. The result
  // type is a client type; the hint is a pure view shape, so the mapping lives
  // here (the components stay free of the client import).
  function addHintFor(r: AddPasskeyResult | null): PasskeyAddHint | null {
    if (!r) return null;
    if (r.ok) return { tone: "ok", text: account_device_added() };
    const map: Record<string, string> = {
      cancelled: account_cancelled(),
      "reauth-required": account_reauth_failed(),
      "duplicate-credential": account_passkey_duplicate(),
      "rate-limited": account_rate_limited(),
      // Same wrapper family as the auth pages: the unknown arm never carries
      // actionable server copy -- qualitative hint, raw message to console.
      unknown:
        (console.error("[add device] unknown failure:", r.message),
        error_unknown()),
    };
    return { tone: "err", text: map[r.reason] ?? error_unknown() };
  }
</script>

<svelte:head><title>{account_menu()}</title></svelte:head>

<!-- The shell's mobile top row carries this page's heading below md (title prop in RootLayout); this container's padding is plain -- the shell row owns the safe-area inset. -->
<div class="h-full overflow-y-auto">
  <div class="px-2 py-4 md:p-6 max-w-2xl space-y-6">
    <h1 class="text-xl font-semibold hidden md:block">
      {account_menu()}
    </h1>

    {#if mode === "online" && archeionSession.user}
      {@const me = archeionSession.user}
      <section class="space-y-3">
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

      <PasskeyManager
        passkeys={passkeyCards}
        phase={passkeyPhase}
        error={archeionSession.passkeysError}
        addHint={passkeyAddHint}
        {adding}
        {onAdd}
        onRename={(id, label) => archeionSession.renamePasskey(id, label)}
        onRevoke={(id) => archeionSession.revokePasskey(id)}
        pairingCode={archeionSession.pairingCode}
        pairingError={archeionSession.pairingError}
        {minting}
        {onMint}
        onClear={() => (archeionSession.pairingCode = null)}
      />

      <EmailManager />

      <LinkedAccounts />
    {/if}

    <!-- The way out: tokens, preferences, then the mode action. -->
    <nav
      class="card preset-tonal-surface divide-y divide-surface-200-800"
      aria-label={account_menu()}
    >
      {#if mode === "online"}
        <HubRow
          href="/account/tokens"
          Icon={KeyRound}
          label={account_tokens()}
        />
      {/if}
      <HubRow href="/settings" Icon={Settings} label={settings_heading()} />
      {#if mode === "online"}
        <HubRow
          onclick={() => void logout()}
          Icon={LogOut}
          label={account_logout()}
        />
      {/if}
    </nav>
  </div>
</div>
