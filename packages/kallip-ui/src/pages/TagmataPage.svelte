<script lang="ts">
  import { agoraSession } from "../lib/session/agora.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { realtimeStore } from "../lib/session/realtime.svelte";
  import type { TagmaCardProps } from "../lib/tagmata.svelte.ts";
  import TagmataDashboard from "../components/tagmata/TagmataDashboard.svelte";
  import {
    tagmata_title,
    rooms_couldnt_reach,
    rooms_retrying,
    common_loading,
  } from "../paraglide/messages.js";

  // The registry is fetched by RootLayout's user_id $effect (it drives
  // auto-connect, so it must load regardless of which page the user lands on,
  // and re-load on re-login); this view reads it reactively. Liveness (the
  // online dot) is NOT here -- it is pushed by realtime's SSE presence and
  // overlaid per-card below.

  // The registry's enrolled cards joined with realtime presence + status: the
  // sole place both are derived. While realtime has not yet resolved (the SSE
  // snapshot is in flight), show "checking" rather than a misleading default
  // "offline"; once resolved, map the presence set to online/offline. The
  // status snapshot overlays as-is (`undefined` while none has arrived, which
  // the card reads as "hide the status line").
  const enrolled = $derived(
    agoraSession.enrolledCards.map(
      (c): TagmaCardProps => ({
        ...c,
        presence: realtimeStore.resolved
          ? realtimeStore.has(c.tagmaId)
            ? "online"
            : "offline"
          : "checking",
        status: realtimeStore.statusFor(c.tagmaId),
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
</script>

<svelte:head><title>{tagmata_title()}</title></svelte:head>

{#if agoraSession.user}
  <TagmataDashboard
    pending={agoraSession.pending}
    {enrolled}
    {phase}
    busy={agoraSession.minting}
    onMint={() => agoraSession.mintTagma()}
    onRevoke={async (id) => {
      await agoraSession.revokeTagma(id);
      // Tear down the revoked tagma's open channel + purge its cache, so a
      // shared device does not keep the previous user's plaintext transcript.
      channelsStore.closeByTagma(id);
    }}
    onCopyCode={(id, secret) => agoraSession.copySecret(id, secret)}
    onRename={(id, label) => agoraSession.renameTagma(id, label)}
    copiedCodeId={agoraSession.copiedCodeId}
  />
{:else if agoraSession.authError}
  <div class="p-4">
    <p class="text-error-500 dark:text-error-400 text-sm">
      {rooms_couldnt_reach({ error: agoraSession.authError })}
    </p>
    <p class="opacity-60 text-sm">{rooms_retrying()}</p>
  </div>
{:else}
  <div class="p-4"><p class="opacity-60">{common_loading()}</p></div>
{/if}
