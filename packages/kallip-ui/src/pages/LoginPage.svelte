<script lang="ts">
  import { agoraSession } from "../lib/session/agora.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import { isValidEmail } from "../lib/email.ts";
  import type { CeremonyResult } from "@kallipai/kallip-agora-client";
  import Brand from "../components/Brand.svelte";
  import Banner from "../components/Banner.svelte";

  let { next = undefined }: { next?: string } = $props();

  let email = $state("");
  let submitting = $state(false);
  let result: CeremonyResult | null = $state(null);
  // Network/transport error from a submit attempt (e.g. agora unreachable now).
  let error = $state<string | null>(null);

  // The reverse guard (already signed in -> /tagmata) and the forward guard
  // (logged out -> /login) live in <RootLayout>; this page is only reached for a
  // genuinely logged-out user. If whoami failed at boot (agora unreachable),
  // agoraSession.authError is set -- surface it so the user isn't staring at a
  // form they can't submit.
  const notice = $derived(error ?? agoraSession.authError);
  const emailValid = $derived(isValidEmail(email));
  const canSubmit = $derived(emailValid && !submitting);

  function reasonMessage(r: CeremonyResult): string | null {
    if (r.ok) return null;
    switch (r.reason) {
      case "cancelled":
        return "Passkey prompt cancelled.";
      case "rate-limited":
        return "Too many attempts. Wait a moment and try again.";
      default:
        // Unknown includes invalid-credentials (401) -- kept generic so as not
        // to leak which emails exist (closed-beta enumeration residual).
        return r.message ?? "Login failed.";
    }
  }

  async function submit(e: Event) {
    e.preventDefault();
    // Email is the login id. Do NOT lowercase: email.rs treats the local part
    // as case-sensitive, so trimming is the only client-side transform; the
    // user must type exactly the address they registered.
    if (!canSubmit) return;
    submitting = true;
    result = null;
    error = null;
    try {
      const r = await agoraSession.login(email.trim());
      result = r;
      if (r.ok) await navigate(next ?? "/tagmata");
    } catch (e) {
      // A thrown error here is transport-level (agora unreachable); the
      // ceremony's own failures come back as a non-ok result below.
      error = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }
</script>

<svelte:head><title>KallipAI · log in</title></svelte:head>

{#if notice}
  <!-- Floats over the centered form so an agora-unreachable error is visible
       without displacing the fields; the ceremony's own failures render inline
       below. -->
  <Banner floating title={`Couldn't reach the server: ${notice}`} />
{/if}

<div class="flex items-center justify-center min-h-dvh p-4 bg-surface-200">
  <form
    class="w-full max-w-sm space-y-6 p-6 bg-surface-100 border border-surface-200 shadow-sm rounded-xl"
    onsubmit={submit}
  >
    <div class="text-center space-y-1">
      <Brand size="lg" />
      <p class="text-sm opacity-60">Welcome back</p>
    </div>

    <label class="block space-y-1">
      <span class="text-sm opacity-70">
        Email <span class="text-error-500">*</span>
      </span>
      <input
        class="input"
        type="email"
        autocomplete="email"
        placeholder="you@example.com"
        bind:value={email}
        required
      />
      {#if email.length > 0 && !emailValid}
        <span class="text-xs text-error-500">Enter a valid email address.</span>
      {/if}
    </label>

    {#if result && !result.ok}
      <p role="alert" class="text-sm text-error-500">
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
        class="font-medium text-primary-500 hover:underline cursor-pointer"
        >Create account</a
      >
    </p>

    <p class="text-center text-sm">
      <a
        href="/connect"
        class="font-medium text-primary-500 hover:underline cursor-pointer"
        >Offline mode</a
      >
    </p>
  </form>
</div>
