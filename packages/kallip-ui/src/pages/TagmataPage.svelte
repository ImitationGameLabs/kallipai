<script lang="ts">
  import { onMount } from "svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import TagmataDashboard from "../components/tagmata/TagmataDashboard.svelte";

  // Fetch on mount; no interval polling (SSE presence push is a future phase).
  // No manual refresh control yet -- re-fetch on revisit (add back when needed).
  onMount(() => {
    void agoraSession.refreshTagmata();
  });

  // The auth gate (logged-out -> /login) lives in <RootLayout>; this page is
  // only reached when `user` is resolving or set, so it renders the skeleton on
  // `undefined` and the dashboard once resolved.

  const phase = $derived(
    agoraSession.tagmataError
      ? "error"
      : agoraSession.tagmataLoaded
        ? "loaded"
        : "loading",
  );
</script>

<svelte:head><title>KallipAI · tagmata</title></svelte:head>

{#if agoraSession.user}
  <TagmataDashboard
    pending={agoraSession.pending}
    enrolled={agoraSession.tagmaCards}
    {phase}
    busy={agoraSession.minting}
    onMint={() => agoraSession.mintTagma()}
    onRevoke={(id) => agoraSession.revokeTagma(id)}
    onCopyCode={(id, secret) => agoraSession.copySecret(id, secret)}
    onRename={(id, label) => agoraSession.renameTagma(id, label)}
    copiedCodeId={agoraSession.copiedCodeId}
  />
{:else if agoraSession.authError}
  <div class="p-4">
    <p class="text-error-500 text-sm">
      Could not reach the server: {agoraSession.authError}
    </p>
    <p class="opacity-60 text-sm">Retrying...</p>
  </div>
{:else}
  <div class="p-4"><p class="opacity-60">Loading...</p></div>
{/if}
