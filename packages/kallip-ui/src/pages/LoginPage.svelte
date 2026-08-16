<script lang="ts">
  import { onMount } from "svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import { isValidUsername } from "../lib/username.ts";
  import type { CeremonyResult } from "@kallipai/kallip-agora-client";
  import Brand from "../components/Brand.svelte";
  import Banner from "../components/Banner.svelte";
  import OAuthProviderButtons from "../components/OAuthProviderButtons.svelte";

  let { returnPath = undefined }: { returnPath?: string } = $props();

  let username = $state("");
  let submitting = $state(false);
  let result: CeremonyResult | null = $state(null);
  // Network/transport error from a submit attempt (e.g. agora unreachable now).
  let error = $state<string | null>(null);
  // Aborts the background conditional-mediation get() before an explicit
  // ceremony (the username form submit) or on unmount. Two concurrent
  // `navigator.credentials.get()` calls deadlock some browsers (notably
  // Firefox), so the pending discoverable autofill MUST be killed first.
  let discoverableCtl: AbortController | null = null;

  // The reverse guard (already signed in -> /tagmata) and the forward guard
  // (logged out -> /login) live in <RootLayout>; this page is only reached for a
  // genuinely logged-out user. If whoami failed at boot (agora unreachable),
  // agoraSession.authError is set -- surface it so the user isn't staring at a
  // form they can't submit.
  const notice = $derived(error ?? agoraSession.authError);
  const usernameValid = $derived(isValidUsername(username));
  const canSubmit = $derived(usernameValid && !submitting);

  function reasonMessage(r: CeremonyResult): string | null {
    if (r.ok) return null;
    switch (r.reason) {
      case "cancelled":
        return "Passkey prompt cancelled.";
      case "rate-limited":
        return "Too many attempts. Wait a moment and try again.";
      default:
        // Unknown includes invalid-credentials (401) -- kept generic so as not
        // to leak which usernames exist (closed-beta enumeration residual).
        return r.message ?? "Login failed.";
    }
  }

  async function submit(e: Event) {
    e.preventDefault();
    // Username is the login id. The server normalizes (trim + ASCII-lowercase),
    // so the user can type their handle in any case.
    if (!canSubmit) return;
    // Kill the background conditional-mediation get before starting the explicit
    // username ceremony (see discoverableCtl's comment).
    discoverableCtl?.abort();
    submitting = true;
    result = null;
    error = null;
    try {
      const r = await agoraSession.login(username.trim());
      result = r;
      if (r.ok) await navigate(returnPath ?? "/tagmata");
    } catch (e) {
      // A thrown error here is transport-level (agora unreachable); the
      // ceremony's own failures come back as a non-ok result below.
      error = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
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
    agoraSession.refreshOAuthProviders();
    // Track mount state so a discoverable-autofill resolution that lands AFTER
    // the user navigated away (Create account / Add device / Offline) does not
    // rip them back to /tagmata from whatever page they are now on.
    let mounted = true;
    const pk = PublicKeyCredential;
    if (typeof pk === "undefined" || !pk.isConditionalMediationAvailable) {
      return () => {
        mounted = false;
      };
    }
    discoverableCtl = new AbortController();
    const signal = discoverableCtl.signal;
    pk.isConditionalMediationAvailable()
      .then((available) => {
        if (!available || !mounted) return;
        agoraSession.loginDiscoverable(signal).then((r) => {
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

<svelte:head><title>KallipAI · log in</title></svelte:head>

{#if notice}
  <!-- Floats over the centered form so an agora-unreachable error is visible
       without displacing the fields; the ceremony's own failures render inline
       below. -->
  <Banner floating title={`Couldn't reach the server: ${notice}`} />
{/if}

<div class="flex items-center justify-center min-h-dvh p-4 bg-surface-200-800">
  <form
    class="w-full max-w-sm space-y-6 p-6 bg-surface-100-900 border border-surface-200-800 shadow-sm rounded-xl"
    onsubmit={submit}
  >
    <div class="text-center space-y-1">
      <Brand size="lg" />
      <p class="text-sm opacity-60">Welcome back</p>
    </div>

    <OAuthProviderButtons {returnPath} />

    <label class="block space-y-1">
      <span class="text-sm opacity-70">
        Username <span class="text-error-500 dark:text-error-400">*</span>
      </span>
      <input
        class="input"
        type="text"
        autocomplete="username webauthn"
        placeholder="your-handle"
        bind:value={username}
        required
      />
      {#if username.length > 0 && !usernameValid}
        <span class="text-xs text-error-500 dark:text-error-400">Enter a valid username.</span>
      {/if}
    </label>

    {#if result && !result.ok}
      <p role="alert" class="text-sm text-error-500 dark:text-error-400">
        {reasonMessage(result)}
      </p>
    {/if}

    <button
      type="submit"
      class="btn preset-filled-primary-500 w-full"
      disabled={!canSubmit}
    >
      {submitting ? "Signing in…" : "Sign in with passkey"}
    </button>

    <p class="text-center text-sm">
      New here?
      <a
        href="/register"
        class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
        >Create account</a
      >
    </p>

    <p class="text-center text-sm">
      On a new device?
      <a
        href="/pair"
        class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
        >Add this device</a
      >
    </p>

    <p class="text-center text-sm">
      <a
        href="/connect"
        class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
        >Offline mode</a
      >
    </p>
  </form>
</div>
