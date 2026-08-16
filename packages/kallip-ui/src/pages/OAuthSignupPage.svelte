<script lang="ts">
  // The OAuth-signup username step. The callback page stashed the held signup
  // token (a single-use bearer) in sessionStorage after a 202 needs-username
  // finish; this page peeks it and submits a user-chosen username to create the
  // account. The stash is PEEKED (not consumed) on mount so a refresh during the
  // step or between duplicate-username retries still works, and cleared only on
  // success. No display_name field -- consistent with the passkey register flow,
  // the users row's display_name stays null; the provider-supplied display_name
  // lands on the external identity (display-only). If the token is gone (direct
  // navigation / refresh after success), bounce back to register.
  import { onMount } from "svelte";
  import {
    agoraSession,
    clearOAuthSignup,
    peekOAuthSignup,
    type OAuthSignupContext,
  } from "../lib/session/agora.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import { isValidUsername } from "../lib/username.ts";
  import type { OAuthSignupResult } from "@kallipai/kallip-agora-client";
  import Brand from "../components/Brand.svelte";
  import UsernameField from "../components/UsernameField.svelte";

  let ctx: OAuthSignupContext | null = $state(null);
  let username = $state("");
  let submitting = $state(false);
  let result: OAuthSignupResult | null = $state(null);
  let error: string | null = $state(null);

  const normalizedUsername = $derived(username.trim().toLowerCase());
  const canSubmit = $derived(isValidUsername(username) && !submitting);

  // Capitalize the provider id (e.g. "github" -> "Github") for the heading so
  // the user sees which identity they are binding. Called inside {#if ctx},
  // where `ctx` is narrowed to non-null.
  function providerLabel(provider: string): string {
    return provider.charAt(0).toUpperCase() + provider.slice(1);
  }

  function reasonMessage(r: OAuthSignupResult): string | null {
    if (r.ok) return null;
    switch (r.reason) {
      case "duplicate-username":
        return "That username is taken.";
      case "invalid-username":
        return "3-32 chars: a-z 0-9, single hyphens only.";
      case "signup-disabled":
        return "New sign-ups are currently disabled. Try again later.";
      case "rate-limited":
        return "Too many attempts. Wait a moment and try again.";
      default:
        return r.message ?? "Sign-up failed.";
    }
  }

  async function submit(e: Event) {
    e.preventDefault();
    if (!ctx || !canSubmit) return;
    submitting = true;
    result = null;
    error = null;
    try {
      const r = await agoraSession.completeOAuthSignup({
        signupToken: ctx.signupToken,
        username: normalizedUsername,
      });
      result = r;
      if (r.ok) {
        // Drop the stash so a later refresh bounces to /register (the token is
        // single-use server-side anyway); then resume.
        clearOAuthSignup();
        // A brand-new account has nothing to resume to -- the `?next=` that
        // rode the OAuth begin (e.g. /login?next=/settings) is for returning
        // signins, not a first-time signup. Always start at home.
        await navigate("/tagmata");
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  onMount(() => {
    ctx = peekOAuthSignup();
    if (!ctx) {
      // No held token: direct navigation, or a refresh after success (the stash
      // was cleared). Restart from register.
      navigate("/register");
    }
  });
</script>

<svelte:head><title>KallipAI · pick a username</title></svelte:head>

<div class="flex items-center justify-center min-h-dvh p-4 bg-surface-100-900">
  {#if ctx}
    <form
      class="w-full max-w-sm space-y-6 p-6 bg-surface-50-950 border border-surface-200-800 shadow-sm rounded-xl"
      onsubmit={submit}
    >
      <div class="text-center space-y-1">
        <Brand size="lg" />
        <p class="text-sm opacity-60">
          Pick a username to finish signing up with {providerLabel(ctx.provider)}
        </p>
      </div>

      <UsernameField bind:value={username} />

      {#if result && !result.ok}
        <p role="alert" class="text-sm text-error-500 dark:text-error-400">
          {reasonMessage(result)}
        </p>
      {/if}
      {#if error}
        <p role="alert" class="text-sm text-error-500 dark:text-error-400">{error}</p>
      {/if}

      <button
        type="submit"
        class="btn preset-filled-primary-500 w-full"
        disabled={!canSubmit}
      >
        {submitting ? "Creating…" : "Create account"}
      </button>

      <p class="text-center text-sm">
        <a
          href="/register"
          class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
          >Use a different sign-in method</a
        >
      </p>
    </form>
  {:else}
    <div class="w-full max-w-sm p-6 text-center">
      <Brand size="lg" />
      <p class="text-sm opacity-60 mt-2">Redirecting…</p>
    </div>
  {/if}
</div>
