<script lang="ts">
  // The "Continue with X" provider buttons rendered above the passkey form on
  // the login and register pages. Reads the configured-provider list from the
  // store and navigates to the chosen provider's authorize URL on click.
  // Extracted so the two pages cannot drift apart. `returnPath` is the
  // sanitized path to resume to
  // after a signin; register has none (a brand-new account always lands on
  // /tagmata).
  import { archeionSession } from "../lib/session/archeion.svelte";
  import {
    auth_couldnt_reach,
    auth_continue_with,
    auth_or,
  } from "../paraglide/messages.js";
  let { returnPath = undefined }: { returnPath?: string } = $props();

  // A begin failure (archeion unreachable, 429) would otherwise reject unhandled:
  // the success path navigates away (page unloads), so only the error path
  // needs handling. Mirrors LinkedAccounts' link-error discipline.
  let error = $state<string | null>(null);
  // Google's OAuth requires https off localhost; a non-secure context
  // (plain http on a LAN host) hides that entry, GitHub stays.
  const providers = $derived(
    window.isSecureContext
      ? archeionSession.oauthProviders
      : archeionSession.oauthProviders.filter((p) => p.id !== "google"),
  );

  async function begin(provider: string): Promise<void> {
    error = null;
    try {
      await archeionSession.signInWithOAuth(provider, returnPath);
    } catch (e) {
      // Transport-level (archeion unreachable); the redirect never happens on
      // failure, so the raw error is console-only and the buttons stay usable.
      console.error("[oauth] begin failed:", e);
      error = auth_couldnt_reach();
    }
  }
</script>

{#if providers.length > 0}
  <div class="space-y-2">
    {#each providers as p (p.id)}
      <button
        type="button"
        class="btn btn-sm preset-outlined-primary-500 hover:preset-filled-primary-500 w-full"
        onclick={() => begin(p.id)}
      >
        {auth_continue_with({ label: p.label })}
      </button>
    {/each}
    <div class="flex items-center gap-2 text-xs opacity-40 py-1">
      <span class="flex-1 border-t border-surface-300-700"></span>
      <span>{auth_or()}</span>
      <span class="flex-1 border-t border-surface-300-700"></span>
    </div>
    {#if error}
      <p role="alert" class="text-xs text-error-500 dark:text-error-400">
        {error}
      </p>
    {/if}
  </div>
{/if}
