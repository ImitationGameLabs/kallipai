<script lang="ts">
  import { onMount } from "svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import { isValidUsername } from "../lib/username.ts";
  import type { CeremonyResult } from "@kallipai/kallip-agora-client";
  import Brand from "../components/Brand.svelte";
  import Banner from "../components/Banner.svelte";
  import OAuthProviderButtons from "../components/OAuthProviderButtons.svelte";
  import UsernameField from "../components/UsernameField.svelte";
  import {
    auth_couldnt_reach,
    auth_passkey_cancelled,
    auth_rate_limited,
    auth_username_taken,
    auth_creating,
    auth_create_account,
    auth_offline_mode,
    register_title,
    register_subtitle,
    register_display_name,
    register_failed,
    register_create_passkey,
    register_have_account,
    auth_sign_in,
  } from "../paraglide/messages.js";

  // display_name length cap enforced on the trimmed value by the agora
  // (auth.rs:179). HTML maxlength counts untrimmed length, so this is a UX
  // hint only -- the server remains the authority.
  const DISPLAY_NAME_MAX = 64;
  let username = $state("");
  let displayName = $state("");
  let submitting = $state(false);
  let result: CeremonyResult | null = $state(null);
  // Network/transport error from a submit attempt (e.g. agora unreachable now).
  let error = $state<string | null>(null);

  // The reverse guard (already signed in -> /tagmata) lives in <RootLayout>.
  // If whoami failed at boot (agora unreachable), surface it proactively.
  const notice = $derived(error ?? agoraSession.authError);

  // Client normalization so the user sees the canonical handle, not a 400 round-trip.
  const normalizedUsername = $derived(username.trim().toLowerCase());
  const usernameValid = $derived(isValidUsername(username));
  const canSubmit = $derived(usernameValid && !submitting);

  // Human copy for each ceremony failure reason.
  function reasonMessage(r: CeremonyResult): string | null {
    if (r.ok) return null;
    switch (r.reason) {
      case "cancelled":
        return auth_passkey_cancelled();
      case "duplicate-username":
        return auth_username_taken();
      case "rate-limited":
        return auth_rate_limited();
      default:
        return r.message ?? register_failed();
    }
  }

  async function submit(e: Event) {
    e.preventDefault();
    if (!canSubmit) return;
    submitting = true;
    result = null;
    error = null;
    try {
      const trimmedDisplay = displayName.trim();
      const r = await agoraSession.register({
        username: normalizedUsername,
        // Omit when blank: the agora falls back to the username as the
        // WebAuthn displayName.
        ...(trimmedDisplay ? { display_name: trimmedDisplay } : {}),
      });
      result = r;
      if (r.ok) await navigate("/tagmata");
    } catch (e) {
      // Transport-level (agora unreachable); ceremony failures are non-ok results.
      error = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  // Fetch enabled OAuth providers for the "Continue with X" buttons. Registration
  // has no return path -- a brand-new account always lands on /tagmata.
  onMount(() => {
    agoraSession.refreshOAuthProviders();
  });
</script>

<svelte:head><title>{register_title()}</title></svelte:head>

{#if notice}
  <!-- Floats over the centered form so an agora-unreachable error is visible
       without displacing the fields; the ceremony's own failures render inline
       below. -->
  <Banner floating title={auth_couldnt_reach({ notice })} />
{/if}

<div class="flex items-center justify-center min-h-dvh p-4 bg-surface-100-900">
  <form
    class="w-full max-w-sm space-y-6 p-6 bg-surface-50-950 border border-surface-200-800 shadow-sm rounded-xl"
    onsubmit={submit}
  >
    <div class="text-center space-y-1">
      <Brand size="lg" />
      <p class="text-sm opacity-60">{register_subtitle()}</p>
    </div>

    <OAuthProviderButtons />

    <UsernameField bind:value={username} />

    <label class="block space-y-1">
      <span class="text-sm opacity-70">{register_display_name()}</span>
      <input
        class="input"
        autocomplete="name"
        maxlength={DISPLAY_NAME_MAX}
        bind:value={displayName}
      />
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
      {submitting ? auth_creating() : register_create_passkey()}
    </button>

    <p class="text-center text-sm">
      {register_have_account()}
      <a
        href="/login"
        class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
        >{auth_sign_in()}</a
      >
      >
    </p>

    <p class="text-center text-sm">
      <a
        href="/connect"
        class="font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
        >{auth_offline_mode()}</a
      >
    </p>
  </form>
</div>
