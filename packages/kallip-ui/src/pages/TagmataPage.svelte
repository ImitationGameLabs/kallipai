<script lang="ts">
  import { onMount } from "svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { realtimeStore } from "../lib/session/realtime.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import type { TagmaCardProps } from "../lib/tagmata.svelte.ts";
  import TagmataDashboard from "../components/tagmata/TagmataDashboard.svelte";

  // Fetch the registry on mount. Liveness (the online dot) is NOT here -- it is
  // pushed by realtime's SSE presence and overlaid per-card below.
  onMount(() => {
    void agoraSession.refreshTagmata();
  });

  // The registry's enrolled cards joined with realtime presence: the sole
  // place presence is derived. While realtime has not yet resolved (the SSE
  // snapshot is in flight), show "checking" rather than a misleading default
  // "offline"; once resolved, map the presence set to online/offline.
  const enrolled = $derived(
    agoraSession.enrolledCards.map(
      (c): TagmaCardProps => ({
        ...c,
        presence: realtimeStore.resolved
          ? realtimeStore.has(c.tagmaId)
            ? "online"
            : "offline"
          : "checking",
      }),
    ),
  );

  const phase = $derived(
    agoraSession.tagmataError
      ? "error"
      : agoraSession.tagmataLoaded
        ? "loaded"
        : "loading",
  );

  // Open an E2EE channel to an enrolled, online tagma's herald, then navigate
  // to its chat view. The full TagmaView (label + online flag) is looked up from
  // the loaded list; the card only carries TagmaCardProps.
  async function onOpenChannel(id: string): Promise<string> {
    const tagma = agoraSession.tagmata.find((t) => t.tagma_id === id);
    if (!tagma) throw new Error("tagma no longer available; refresh the list");
    const convId = await channelsStore.open(tagma);
    await navigate(`/chat/${convId}`);
    return convId;
  }
</script>

<svelte:head><title>KallipAI · tagmata</title></svelte:head>

{#if agoraSession.user}
  <TagmataDashboard
    pending={agoraSession.pending}
    {enrolled}
    {phase}
    busy={agoraSession.minting}
    onMint={() => agoraSession.mintTagma()}
    onRevoke={(id) => agoraSession.revokeTagma(id)}
    onCopyCode={(id, secret) => agoraSession.copySecret(id, secret)}
    onRename={(id, label) => agoraSession.renameTagma(id, label)}
    {onOpenChannel}
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
