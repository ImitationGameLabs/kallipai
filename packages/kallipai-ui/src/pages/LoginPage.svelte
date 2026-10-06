<script lang="ts">
  import { onMount } from "svelte";
  import { archeionSession } from "../lib/session/archeion.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import { isValidUsername } from "../lib/username.ts";
  import type { CeremonyResult } from "@kallipai/kallipai-archeion-client";
  import Brand from "../components/Brand.svelte";
  import FormError from "../components/FormError.svelte";
  import Banner from "../components/Banner.svelte";
  import ReqMark from "../components/ReqMark.svelte";
  import OAuthProviderButtons from "../components/OAuthProviderButtons.svelte";
  import SecretInput from "../components/SecretInput.svelte";
  import {
    auth_couldnt_reach,
    auth_passkey_cancelled,
    auth_rate_limited,
    auth_create_account,
    auth_offline_mode,
    auth_online_mode,
    login_offline_key_label,
    login_offline_key_placeholder,
    login_offline_submit,
    login_offline_failed,
    login_offline_disabled,
    login_passkey_insecure,
    login_title,
    login_welcome_back,
    login_username,
    login_username_placeholder,
    login_username_invalid,
    login_failed,
    login_signing_in,
    login_submit,
    login_new_here,
    login_new_device,
    auth_add_this_device,
  } from "../paraglide/messages.js";

  let {
    returnPath = undefined,
    offlineLogin = false,
  }: { returnPath?: string; offlineLogin?: boolean } = $props();

  let username = $state("");
  let submitting = $state(false);
  let result: CeremonyResult | null = $state(null);
  // Environment error from a submit attempt: archeion unreachable, or an
  // insecure origin where the passkey ceremony cannot run.
  let error = $state<string | null>(null);
  // Aborts the background conditional-mediation get() before an explicit
  // ceremony (the username form submit) or on unmount. Two concurrent
  // `navigator.credentials.get()` calls deadlock some browsers (notably
  // Firefox), so the pending discoverable autofill MUST be killed first.
  let discoverableCtl: AbortController | null = null;
  // Operator-key (offline-branch) login state. The branch itself is
  // deployment-driven: the app shell passes offlineLogin from its build-time
  // injection, so a cloud build never renders it (two-way information
  // hiding). "offline" here names the LOGIN branch only -- the session it
  // produces is the standard online form;
  // it is unrelated to the AppMode offline of the legacy /local family.
  let mode = $state<"online" | "offline">("online");
  let adminKey = $state("");
  let offlineBusy = $state(false);
  let offlineError = $state<string | null>(null);
  let offlineDisabled = $state(false);
  const canKeySubmit = $derived(adminKey.trim().length > 0 && !offlineBusy);

  // Passkeys are a browser secure-context feature: on plain http off-
  // localhost the ceremony cannot run at all, so the passkey path
  // degrades: a hint above the field, then the inline error at submit;
  // the GitHub/admin-key paths below carry the login surface.
  const secureContext = window.isSecureContext;

  // The reverse guard (already signed in -> /tagmata) and the forward guard
  // (logged out -> /login) live in <RootLayout>; this page is only reached for a
  // genuinely logged-out user. If whoami failed at boot (archeion unreachable),
  // archeionSession.authError is set -- the floating banner above carries that
  // environment error, while a submit's own transport failure renders inline
  // in the form below (FormError); the two channels no longer merge.
  const usernameValid = $derived(isValidUsername(username));
  const canSubmit = $derived(usernameValid && !submitting);

  function reasonMessage(r: CeremonyResult): string | null {
    if (r.ok) return null;
    switch (r.reason) {
      case "cancelled":
        return auth_passkey_cancelled();
      case "rate-limited":
        return auth_rate_limited();
      default:
        // Unknown includes invalid-credentials (401) -- kept generic so as not
        // to leak which usernames exist (closed-beta enumeration residual);
        // any raw message is diagnostic noise for the console, not the form.
        if (r.message) console.error("[login] unknown failure:", r.message);
        return login_failed();
    }
  }

  async function submit(e: Event) {
    e.preventDefault();
    // Username is the login id. The server normalizes (trim + ASCII-lowercase),
    // so the user can type their handle in any case.
    if (!canSubmit) return;
    // The passkey ceremony cannot run off a secure context (plain http
    // off localhost): fail at the point of action with the reason rather
    // than a dead-looking disabled form.
    if (!secureContext) {
      error = login_passkey_insecure();
      return;
    }
    // Kill the background conditional-mediation get before starting the explicit
    // username ceremony (see discoverableCtl's comment).
    discoverableCtl?.abort();
    submitting = true;
    result = null;
    error = null;
    try {
      const r = await archeionSession.login(username.trim());
      result = r;
      if (r.ok) await navigate(returnPath ?? "/tagmata");
    } catch (e) {
      // A thrown error here is transport-level (archeion unreachable); the
      // ceremony's own failures come back as a non-ok result below.
      console.error(e);
      error = auth_couldnt_reach();
    } finally {
      submitting = false;
    }
  }

  // Offline-branch submit: one POST, no authenticator step. 401 keeps the
  // form usable (wrong key); 404 means the route is not mounted on this
  // deployment -- surface the disabled copy rather than a generic failure.
  async function submitKey(e: Event) {
    e.preventDefault();
    if (!canKeySubmit) return;
    discoverableCtl?.abort();
    offlineBusy = true;
    offlineError = null;
    try {
      const r = await archeionSession.adminLogin(adminKey.trim());
      if (r.ok) {
        await navigate(returnPath ?? "/tagmata");
      } else if (r.status === 404) {
        offlineDisabled = true;
        offlineError = login_offline_disabled();
      } else {
        offlineError = login_offline_failed();
      }
    } catch (e) {
      console.error(e);
      offlineError = auth_couldnt_reach();
    } finally {
      offlineBusy = false;
    }
  }

  // Discoverable (passwordless) login via conditional-UI autofill: if the
  // browser supports it, kick off a discoverable get on mount. The promise
  // stays pending until the user picks a passkey from the username field's
  // `webauthn` autofill suggestion; on success we navigate. A `cancelled`
  // result means no matching credential / user dismissed -- fall through
  // silently to the username form. Unsupported browsers skip this entirely.
  onMount(() => {
    // Fetch enabled OAuth providers for the "Continue with X" buttons (fire-
    // and-forget; a failure leaves the list empty).
    archeionSession.refreshOAuthProviders();
    // Track mount state so a discoverable-autofill resolution that lands AFTER
    // the user navigated away (Create account / Add device / Offline) does not
    // rip them back to /tagmata from whatever page they are now on.
    let mounted = true;
    // typeof on the bare identifier is the safe probe: referencing
    // PublicKeyCredential directly throws off secure context.
    const pk =
      typeof PublicKeyCredential === "undefined"
        ? undefined
        : PublicKeyCredential;
    if (
      typeof pk === "undefined" ||
      !pk.isConditionalMediationAvailable ||
      !window.isSecureContext
    ) {
      return () => {
        mounted = false;
      };
    }
    discoverableCtl = new AbortController();
    const signal = discoverableCtl.signal;
    pk.isConditionalMediationAvailable()
      .then((available) => {
        if (!available || !mounted) return;
        archeionSession.loginDiscoverable(signal).then((r) => {
          if (!mounted) return;
          if (r.ok) {
            // The username form may have won the race (user typed + submitted
            // while autofill was pending) -- defer to it then.
            if (submitting) return;
            navigate(returnPath ?? "/tagmata");
          } else if (r.reason !== "cancelled") {
            // A real failure (rate-limited, transport) -- surface it inline.
            result = r;
          }
        });
      })
      .catch(() => {
        // Conditional-UI availability check rejected: unsupported environment,
        // fall through silently to the username form.
      });
    return () => {
      mounted = false;
      // Abort any pending conditional get so it cannot overlap a ceremony on the
      // next mounted page or linger after navigation.
      discoverableCtl?.abort();
    };
  });
</script>

<svelte:head><title>{login_title()}</title></svelte:head>
{#if archeionSession.authError}
  <!-- Environment error (archeion unreachable at boot): stays in the floating
       banner; a submit's own failures render inline in the form below. -->
  <Banner floating title={archeionSession.authError} />
{/if}

<div class="flex items-center justify-center min-h-dvh p-4 bg-surface-200-800">
  <form
    class="w-full max-w-sm space-y-6 p-6 bg-surface-100-900 border border-surface-200-800 shadow-sm rounded-xl"
    onsubmit={mode === "offline" ? submitKey : submit}
  >
    <div class="text-center space-y-1">
      <Brand size="lg" />
      <p class="text-sm opacity-60">{login_welcome_back()}</p>
    </div>

    {#if mode === "offline"}
      <label class="block space-y-1">
        <span class="text-sm opacity-70">
          {login_offline_key_label()}
          <ReqMark />
        </span>
        <!-- The admin key is a secret, not a password: SecretInput keeps
             it out of the browser credential heuristics. -->
        <SecretInput
          bind:value={adminKey}
          placeholder={login_offline_key_placeholder()}
          disabled={offlineDisabled}
          aria-required="true"
        />
      </label>
      {#if offlineError}
        <FormError message={offlineError} />
      {/if}
      <button
        type="submit"
        class="btn preset-filled-primary-500 w-full"
        disabled={!canKeySubmit || offlineDisabled}
      >
        {offlineBusy ? login_signing_in() : login_offline_submit()}
      </button>
      <p class="text-center text-sm">
        <button
          type="button"
          class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
          onclick={() => (mode = "online")}>{auth_online_mode()}</button
        >
      </p>
    {:else}
      <OAuthProviderButtons {returnPath} />

      {#if !secureContext && !error}
        <p class="text-xs opacity-70">{login_passkey_insecure()}</p>
      {/if}

      <label class="block space-y-1">
        <span class="text-sm opacity-70">
          {login_username()}
          <ReqMark />
        </span>
        <input
          class="input"
          type="text"
          autocomplete="username webauthn"
          placeholder={login_username_placeholder()}
          bind:value={username}
          required
        />
        {#if username.length > 0 && !usernameValid}
          <span class="text-xs text-error-500 dark:text-error-400"
            >{login_username_invalid()}</span
          >
        {/if}
      </label>
      {#if error}
        <FormError message={error} />
      {:else if result && !result.ok}
        <FormError message={reasonMessage(result)} />
      {/if}

      <button
        type="submit"
        class="btn preset-filled-primary-500 w-full"
        disabled={!canSubmit}
      >
        {submitting ? login_signing_in() : login_submit()}
      </button>

      <p class="text-center text-sm">
        {login_new_here()}
        <a
          href="/register"
          class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
          >{auth_create_account()}</a
        >
      </p>

      <p class="text-center text-sm">
        {login_new_device()}
        <a
          href="/pair"
          class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
          >{auth_add_this_device()}</a
        >
        >
      </p>

      {#if offlineLogin}
        <p class="text-center text-sm">
          <button
            type="button"
            class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
            onclick={() => (mode = "offline")}>{auth_offline_mode()}</button
          >
        </p>
      {/if}
    {/if}
  </form>
</div>
