<script lang="ts">
  import { sessionStore } from "../lib/session/session.svelte";
  import Composer from "../components/Composer.svelte";
  import TranscriptView from "../components/TranscriptView.svelte";
  import { createComposer } from "../lib/composer.svelte.ts";

  // Single composer instance shared by the empty-state prompt chips (which write
  // into the draft and focus the field) and the composer input itself.
  const composer = createComposer({
    send: (text) => sessionStore.send(text),
    canSubmit: () => sessionStore.connected && !sessionStore.connecting,
  });

  // The composer is disabled unless a session is live and idle.
  const disabled = $derived(!sessionStore.connected || sessionStore.connecting);
</script>

<svelte:head><title>KallipAI · chat</title></svelte:head>

{#if sessionStore.session}
  <div class="flex flex-col h-full">
    <div class="flex-1 min-h-0">
      <TranscriptView lines={sessionStore.transcript.lines} {composer} />
    </div>
    <Composer
      {composer}
      {disabled}
      busy={sessionStore.busy}
      pendingCount={sessionStore.pending.length}
      oninterrupt={() => sessionStore.interrupt()}
    />
  </div>
{/if}
