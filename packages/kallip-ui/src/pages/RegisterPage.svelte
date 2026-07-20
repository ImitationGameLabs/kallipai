<script lang="ts">
  import { onMount } from "svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import { isValidEmail } from "../lib/email.ts";
  import { isValidUsername } from "../lib/username.ts";
  import type { CeremonyResult } from "@kallipai/kallip-agora-client";
  import Brand from "../components/Brand.svelte";
  import Banner from "../components/Banner.svelte";

  // display_name length cap enforced on the trimmed value by the agora
  // (auth.rs:179). HTML maxlength counts untrimmed length, so this is a UX
  // hint only -- the server remains the authority.
  const DISPLAY_NAME_MAX = 64;
  // An invite link can pre-fill the code via ?invite=... so the recipient just
  // picks a username. Seeded once on mount (not reactively bound).
  let { invite: initialInvite = "" }: { invite?: string } = $props();
  let invite = $state("");
  let email = $state("");
  let username = $state("");
  let displayName = $state("");
  let submitting = $state(false);
  let result: CeremonyResult | null = $state(null);
  // Network/transport error from a submit attempt (e.g. agora unreachable now).
  let error = $state<string | null>(null);

  // The reverse guard (already signed in -> /tagmata) lives in <RootLayout>.
  // If whoami failed at boot (agora unreachable), surface it proactively.
  const notice = $derived(error ?? agoraSession.authError);

  onMount(() => {
    if (initialInvite) invite = initialInvite;
  });

  // Client normalization so the user sees the canonical handle, not a 400 round-trip.
  const normalizedUsername = $derived(username.trim().toLowerCase());
  const usernameValid = $derived(isValidUsername(username));
  const emailValid = $derived(isValidEmail(email));
  const canSubmit = $derived(
    invite.trim().length > 0 && emailValid && usernameValid && !submitting,
  );

  // Human copy for each ceremony failure reason.
  function reasonMessage(r: CeremonyResult): string | null {
    if (r.ok) return null;
    switch (r.reason) {
      case "cancelled":
        return "Passkey prompt cancelled.";
      case "invalid-invite":
        return "That invite code is invalid or already used.";
      case "duplicate-email":
        return "That email is already registered.";
      case "duplicate-username":
        return "That username is taken.";
      case "rate-limited":
        return "Too many attempts. Wait a moment and try again.";
      default:
        return r.message ?? "Registration failed.";
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
        invite_code: invite.trim(),
        email: email.trim(),
        username: normalizedUsername,
        // Omit when blank: the agora falls back to the username as the
        // WebAuthn displayName (auth.rs:199-209).
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
</script>

<svelte:head><title>KallipAI · register</title></svelte:head>

{#if notice}
  <!-- Floats over the centered form so an agora-unreachable error is visible
       without displacing the fields; the ceremony's own failures render inline
       below. -->
  <Banner floating title={`Couldn't reach the server: ${notice}`} />
{/if}

<div class="flex items-center justify-center min-h-dvh p-4 bg-surface-100">
  <form
    class="w-full max-w-sm space-y-6 p-6 bg-surface-50 border border-surface-200 shadow-sm rounded-xl"
    onsubmit={submit}
  >
    <div class="text-center space-y-1">
      <Brand size="lg" />
      <p class="text-sm opacity-60">Create your account</p>
    </div>

    <label class="block space-y-1">
      <span class="text-sm opacity-70">
        Invite code <span class="text-error-500">*</span>
      </span>
      <input
        class="input"
        autocomplete="off"
        placeholder="sk-invite-..."
        bind:value={invite}
        required
      />
    </label>

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

    <label class="block space-y-1">
      <span class="text-sm opacity-70">
        Username <span class="text-error-500">*</span>
      </span>
      <input
        class="input"
        autocomplete="username"
        placeholder="a-z, 0-9, -, 3-32 chars"
        bind:value={username}
        required
      />
      {#if username.length > 0 && !usernameValid}
        <span class="text-xs text-error-500"
          >3-32 chars: a-z 0-9, single hyphens only (no
          leading/trailing/consecutive)</span
        >
      {/if}
    </label>

    <label class="block space-y-1">
      <span class="text-sm opacity-70">Display name</span>
      <input
        class="input"
        autocomplete="name"
        maxlength={DISPLAY_NAME_MAX}
        bind:value={displayName}
      />
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
      {submitting ? "Creating…" : "Create passkey"}
    </button>

    <p class="text-center text-sm">
      Already have one?
      <a
        href="/login"
        class="font-medium text-primary-500 hover:underline cursor-pointer"
        >Sign in</a
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
