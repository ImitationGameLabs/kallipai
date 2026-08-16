<script lang="ts">
  // The OAuth redirect-back landing page. The provider redirects here with
  // `?code=...&state=...` (read from the URL by each app's thin route wrapper
  // and passed as props). The page exchanges them via the store, then navigates
  // by the result kind: a signin resumes to `returnPath`/home, a link returns to
  // settings, a needs-username (unlinked identity) continues to the username
  // step. A failure renders inline + a return-to-login link.
  import { onMount } from "svelte";
  import { agoraSession, stashOAuthSignup } from "../lib/session/agora.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import Brand from "../components/Brand.svelte";
  import {
    oauth_callback_title,
    oauth_missing_data,
    oauth_signin_failed,
    oauth_completing,
    oauth_back_to_signin,
  } from "../paraglide/messages.js";

  let { code, oauthState }: { code?: string; oauthState?: string } = $props();

  let busy = $state(true);
  let error: string | null = $state(null);

  onMount(async () => {
    if (!code || !oauthState) {
      busy = false;
      error = oauth_missing_data();
      return;
    }
    try {
      const result = await agoraSession.completeOAuthFromCallback(
        code,
        oauthState,
      );
      if (!result.ok) {
        busy = false;
        error = result.message ?? oauth_signin_failed();
        return;
      }
      if (result.kind === "signin") {
        await navigate(result.returnPath ?? "/tagmata");
      } else if (result.kind === "needs-username") {
        // Unlinked identity: hold the claim in sessionStorage (NOT the URL --
        // the signup token is a single-use bearer) and continue to the
        // username step.
        stashOAuthSignup({
          signupToken: result.signupToken,
          provider: result.provider,
        });
        await navigate("/auth/signup");
      } else {
        // A completed link returns to settings (the user is already signed in).
        await navigate("/settings");
      }
    } catch (e) {
      busy = false;
      error = e instanceof Error ? e.message : String(e);
    }
  });
</script>

<svelte:head><title>{oauth_callback_title()}</title></svelte:head>

<div class="flex items-center justify-center min-h-dvh p-4">
  <div class="w-full max-w-sm space-y-4 p-6 text-center">
    <Brand size="lg" />
    {#if busy}
      <p class="text-sm opacity-60">{oauth_completing()}</p>
    {:else}
      <p class="text-sm text-error-500 dark:text-error-400">{error}</p>
      <a
        href="/login"
        class="inline-block font-medium text-primary-500 dark:text-primary-400 hover:underline cursor-pointer"
        >{oauth_back_to_signin()}</a
      >
    {/if}
  </div>
</div>
