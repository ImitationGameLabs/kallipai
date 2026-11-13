<script lang="ts">
  // The access-token page: a list with its create form, permission editing, a
  // one-time secret panel and revocation. Separate from /account because the
  // surface is long and its copy (permission lists, expiry choices) answers a
  // different question than the identity page does.
  import AccessTokens from "../../components/account/AccessTokens.svelte";
  import { archeionSession } from "../../lib/session/archeion.svelte";
  import { shellMode } from "../../lib/shell/port.ts";
  import { account_tokens } from "../../paraglide/messages.js";

  // Branch on mode, not on `user`: offline must never act on a stale session.
  const mode = $derived(shellMode());

  // One list fetch when the signed-in user resolves. The section re-renders
  // from the store; the loaded flag keeps a transient failure from refetching
  // on every render.
  $effect(() => {
    if (
      mode === "online" &&
      archeionSession.user &&
      !archeionSession.tokensLoaded
    ) {
      archeionSession.refreshTokens();
    }
  });
</script>

<svelte:head><title>{account_tokens()}</title></svelte:head>

<!-- The shell's mobile top row carries this page's heading below md (title prop in RootLayout); this container's padding is plain -- the shell row owns the safe-area inset. -->
<div class="h-full overflow-y-auto">
  <div class="px-2 py-4 md:p-6 max-w-2xl space-y-6">
    <h1 class="text-xl font-semibold hidden md:block">
      {account_tokens()}
    </h1>

    {#if mode === "online" && archeionSession.user}
      <AccessTokens />
    {/if}
  </div>
</div>
