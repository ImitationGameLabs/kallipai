<script lang="ts">
  // The "Continue with X" provider buttons rendered above the passkey form on
  // the login and register pages. Reads the configured-provider list from the
  // store and navigates to the chosen provider's authorize URL on click.
  // Extracted so the two pages cannot drift apart (they previously diverged on
  // the divider border shade). `returnPath` is the sanitized path to resume to
  // after a signin; register has none (a brand-new account always lands on
  // /tagmata).
  import { agoraSession } from "../lib/session/agora.svelte";

  let { returnPath = undefined }: { returnPath?: string } = $props();

  // A begin failure (agora unreachable, 429) would otherwise reject unhandled:
  // the success path navigates away (page unloads), so only the error path
  // needs handling. Mirrors LinkedAccounts' link-error discipline.
  let error = $state<string | null>(null);

  async function begin(provider: string): Promise<void> {
    error = null;
    try {
      await agoraSession.signInWithOAuth(provider, returnPath);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
</script>

{#if agoraSession.oauthProviders.length > 0}
  <div class="space-y-2">
    {#each agoraSession.oauthProviders as p (p.id)}
      <button
        type="button"
        class="btn btn-sm preset-tonal-surface w-full"
        onclick={() => begin(p.id)}
      >
        Continue with {p.label}
      </button>
    {/each}
    <div class="flex items-center gap-2 text-xs opacity-40 py-1">
      <span class="flex-1 border-t border-surface-300-700"></span>
      <span>or</span>
      <span class="flex-1 border-t border-surface-300-700"></span>
    </div>
    {#if error}
      <p role="alert" class="text-xs text-error-500 dark:text-error-400">{error}</p>
    {/if}
  </div>
{/if}
