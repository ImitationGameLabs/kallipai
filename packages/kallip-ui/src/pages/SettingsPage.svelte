<script lang="ts">
  import { archeionSession } from "../lib/session/archeion.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { configStore } from "../lib/config/config.svelte";
  import { shellMode } from "../lib/shell/port.ts";
  import { managementBackend } from "../lib/manage/client.ts";
  import { KallipError } from "@kallipai/kallip-common";
  import {
    timezoneLoad,
    timezoneSetting,
    saveTimezoneSetting,
    timezoneCandidates,
  } from "../lib/time/stamp.svelte.ts";
  import type {
    AddPasskeyResult,
    PasskeySummary,
    ProviderRequest,
  } from "@kallipai/kallip-archeion-client";
  import type {
    PasskeyAddHint,
    PasskeyCardProps,
    PasskeyPhase,
  } from "../lib/passkeys.svelte.ts";
  import PasskeyManager from "../components/settings/PasskeyManager.svelte";
  import LinkedAccounts from "../components/settings/LinkedAccounts.svelte";
  import EmailManager from "../components/settings/EmailManager.svelte";
  import ProviderVault from "../components/settings/ProviderVault.svelte";
  import LightSwitch from "../components/LightSwitch.svelte";
  import LanguageSwitch from "../components/LanguageSwitch.svelte";
  import { Switch } from "@skeletonlabs/skeleton-svelte";
  import {
    notificationPermission,
    requestNotificationPermission,
    type PermissionState,
  } from "../lib/session/notify.ts";
  import {
    nav_tagmata,
    settings_appearance,
    settings_dark_mode,
    settings_language,
    settings_timezone,
    settings_timezone_desc,
    settings_timezone_placeholder,
    settings_timezone_saved,
    settings_timezone_clear,
    settings_timezone_invalid,
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
    chat_notifications_label,
    common_save,
    chat_notifications_desc,
    chat_notifications_denied,
  } from "../paraglide/messages.js";

  // Settings is now info-only: account actions (logout, mode switch) live in
  // the sidebar AccountMenu. Online shows the account (identity lives in
  // archeion); offline shows the tagma connection (no identity). Offline
  // Disconnect/Reconnect stays here -- it is tagma session management, not an
  // account/mode action.
  const mode = $derived(shellMode());
  const offlineUrl = $derived(configStore.value?.offline?.tagmaUrl ?? "");

  // -- notifications (all hosts; one web+tauri switch) ------
  // The toggle click IS the user gesture: enabling requests permission right
  // here (MDN: browsers drop prompt-less requests), and only a granted
  // answer persists the switch. Denied is a browser-level terminal state --
  // the guidance line (rendered proactively, not only after a failed
  // request) points at the browser's site settings, the only way back.
  const notificationsEnabled = $derived(
    configStore.value?.notificationsEnabled === true,
  );
  let notifyPermission = $state<PermissionState>("default");
  $effect(() => {
    void notificationPermission().then((s) => (notifyPermission = s));
  });
  async function onNotificationsToggle(event: { checked: boolean }) {
    if (!event.checked) {
      await configStore.setNotificationsEnabled(false);
      return;
    }
    notifyPermission = await requestNotificationPermission();
    if (notifyPermission === "granted") {
      await configStore.setNotificationsEnabled(true);
    }
  }
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
      archeionSession.user &&
      !archeionSession.passkeysLoaded
    ) {
      archeionSession.refreshPasskeys();
    }
  });

  // The provider vault loads the same way (mirrors the passkeys discipline:
  //   a list failure lands in `providersError` without blanking `user`).
  $effect(() => {
    if (
      mode === "online" &&
      archeionSession.user &&
      !archeionSession.providersLoaded
    ) {
      archeionSession.refreshProviders();
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

  // Project the wire types into the prop-driven components. The store owns the
  // ceremony + mutations; this page only projects state and forwards callbacks.
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
    if (r.ok) return { tone: "ok", text: settings_device_added() };
    const map: Record<string, string> = {
      cancelled: settings_cancelled(),
      "reauth-required": settings_reauth_failed(),
      "duplicate-credential": settings_passkey_duplicate(),
      "rate-limited": settings_rate_limited(),
      // Same wrapper family as the auth pages: the unknown arm never carries
      // actionable server copy -- qualitative hint, raw message to console.
      unknown:
        (console.error("[add device] unknown failure:", r.message),
        settings_error_unknown()),
    };
    return { tone: "err", text: map[r.reason] ?? settings_error_unknown() };
  }

  // -- provider vault (online only) ---------------------------------------
  // Thin forwarders: the store owns every ceremony and mutation (create
  // seals encrypted keys; rename re-sends the row unchanged except the
  // name; flip/reveal open with THIS device's vault key). The components
  // render and surface errors; this page only wires store calls in.
  async function onCreateProvider(req: ProviderRequest): Promise<boolean> {
    await archeionSession.createProvider(req);
    return true;
  }

  // -- timezone (offline management face) ---------------------------------
  // The tagma-level display timezone: what every absolute stamp renders in.
  // The server validates IANA names (400 on a miss); the input is free text
  // with a browser-provided candidate list, so no zone catalogue ships in
  // the bundle.
  let tzSeeded = $state(false);
  let tzInput = $state("");
  const tzCandidates = timezoneCandidates();
  let tzBusy = $state(false);
  let tzFeedback = $state<"saved" | "invalid" | "network" | null>(null);

  $effect(() => {
    if (mode !== "offline" || !timezoneLoad.done || !timezoneLoad.ok) return;
    if (!tzSeeded) {
      tzInput = timezoneSetting.value ?? "";
      tzSeeded = true;
    }
  });
  async function onTimezoneSave(): Promise<void> {
    const name = tzInput.trim();
    tzBusy = true;
    try {
      const confirmed = await saveTimezoneSetting(
        (tz) => managementBackend().putTimezone(tz),
        name === "" ? null : name,
      );
      tzInput = confirmed ?? "";
      tzFeedback = "saved";
    } catch (e) {
      tzFeedback =
        e instanceof KallipError && e.api.status === 400
          ? "invalid"
          : "network";
    } finally {
      tzBusy = false;
    }
  }
</script>

<svelte:head><title>{settings_title()}</title></svelte:head>

<div class="h-full overflow-y-auto">
  <div class="px-2 py-4 md:p-6 max-w-md space-y-6">
    <h1 class="text-xl font-semibold hidden md:block">
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

    {#if mode === "offline"}
      <section class="space-y-3">
        <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
          {settings_timezone()}
        </h2>
        <div class="card preset-tonal-surface space-y-2 p-4">
          <p class="text-sm opacity-70">{settings_timezone_desc()}</p>
          <input
            class="input text-sm"
            type="text"
            list="timezone-candidates"
            bind:value={tzInput}
            placeholder={settings_timezone_placeholder()}
            aria-label={settings_timezone()}
          />
          <datalist id="timezone-candidates">
            {#each tzCandidates as zone (zone)}
              <option value={zone}></option>
            {/each}
          </datalist>
          <div class="flex items-center gap-2">
            <button
              class="btn preset-filled-primary-500 text-sm"
              disabled={tzBusy || !timezoneLoad.ok}
              onclick={onTimezoneSave}
            >
              {common_save()}
            </button>
            <button
              class="btn preset-outlined-surface-500 hover:preset-filled-surface-500 text-sm"
              disabled={tzBusy || !timezoneLoad.ok}
              onclick={() => {
                tzInput = "";
                void onTimezoneSave();
              }}
            >
              {settings_timezone_clear()}
            </button>
          </div>
          {#if tzFeedback === "saved"}
            <p class="text-sm preset-text-success-500">
              {settings_timezone_saved()}
            </p>
          {:else if tzFeedback === "invalid"}
            <p class="text-sm preset-text-error-500">
              {settings_timezone_invalid()}
            </p>
          {:else if tzFeedback === "network"}
            <p class="text-sm preset-text-error-500">
              {settings_error_unknown()}
            </p>
          {/if}
        </div>
      </section>
    {/if}

    <section class="space-y-3">
      <h2 class="text-sm font-medium uppercase opacity-60 tracking-wide">
        {chat_notifications_label()}
      </h2>
      <div class="card preset-tonal-surface space-y-2 p-4">
        <div class="flex items-center justify-between gap-3">
          <p class="text-sm">{chat_notifications_desc()}</p>
          <Switch
            checked={notificationsEnabled}
            onCheckedChange={onNotificationsToggle}
          >
            <Switch.Control>
              <Switch.Thumb />
            </Switch.Control>
            <Switch.HiddenInput aria-label={chat_notifications_label()} />
          </Switch>
        </div>
        {#if notifyPermission === "denied"}
          <p class="text-error-500 dark:text-error-400 text-xs">
            {chat_notifications_denied()}
          </p>
        {/if}
      </div>
    </section>

    {#if mode === "online"}
      {#if archeionSession.user}
        {@const me = archeionSession.user}
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

        <ProviderVault
          entries={archeionSession.providers}
          phase={archeionSession.providersError
            ? "error"
            : archeionSession.providersLoaded
              ? "loaded"
              : "loading"}
          error={archeionSession.providersError}
          canFlip={archeionSession.canFlipKeys()}
          onRename={(id, name) => archeionSession.renameProvider(id, name)}
          onFlip={(entry) => archeionSession.flipProviderEncryption(entry)}
          onDelete={(id) => archeionSession.deleteProvider(id)}
          onCreate={onCreateProvider}
          onCopyKey={(entry) => archeionSession.revealProviderKey(entry)}
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
                class="btn btn-sm preset-outlined-surface-500 hover:preset-filled-surface-500"
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
