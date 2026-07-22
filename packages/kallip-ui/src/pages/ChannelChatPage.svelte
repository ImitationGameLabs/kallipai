<script lang="ts">
  // One online conversation over an E2EE herald channel (the independent online
  // path -- its own lean transcript view, not the offline TranscriptView). The
  // transport is a RelayChannel owned by channelsStore; this page just renders
  // its ChannelTranscript and feeds sends through the reused Composer input.
  import Composer from "../components/Composer.svelte";
  import { createComposer } from "../lib/composer.svelte.ts";
  import { createAutoScroll } from "../lib/transcript.svelte.ts";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { navigate } from "../lib/shell/port.ts";

  let { convId }: { convId: string } = $props();

  // Resolves to undefined for a deep link to a channel that is not currently
  // open (channelsStore only knows channels opened this session).
  const channelState = $derived(channelsStore.get(convId));

  // The composer is created once; its closures re-read the reactive state on
  // each submit/canSubmit call.
  const composer = createComposer({
    send: (text) => channelsStore.send(convId, text),
    canSubmit: () =>
      channelState?.status === "open" &&
      channelState.transcript.status !== "busy",
  });

  const disabled = $derived(!channelState || channelState.status !== "open");
  const busy = $derived(channelState?.transcript.status === "busy");

  // Reuse the shared stick-to-tail controller (same primitive the offline
  // TranscriptView uses): pins to the bottom as lines arrive, but stops once the
  // user scrolls up to read history.
  const scroll = createAutoScroll();
  $effect(() => {
    void channelState?.transcript.lines.length;
    scroll.stick();
  });
</script>

<svelte:head><title>KallipAI · channel</title></svelte:head>

{#if !channelState}
  <!-- Deep link to a channel that was not opened this session. The convId is a
       server-derived value the client does not reverse-resolve, so route the
       user back to the tagma list rather than guessing. -->
  <div class="h-full grid place-items-center p-6">
    <div class="text-center flex flex-col gap-3 max-w-sm">
      <p class="text-sm opacity-80">
        This channel is not open. Open it from the tagmata list.
      </p>
      <button
        type="button"
        class="btn preset-tonal-surface self-center"
        onclick={() => navigate("/tagmata")}
      >
        Go to tagmata
      </button>
    </div>
  </div>
{:else}
  <div class="flex flex-col h-full">
    <div
      class="flex-1 min-h-0 overflow-auto"
      bind:this={scroll.viewport}
      onscroll={scroll.onScroll}
    >
      <div class="mx-auto w-full max-w-2xl p-4 flex flex-col gap-3">
        {#if channelState.transcript.lines.length === 0 && !busy}
          <p class="text-sm opacity-60 text-center mt-8">
            Send a message to start the conversation.
          </p>
        {/if}
        {#each channelState.transcript.lines as line (line.seq)}
          {#if line.role === "system"}
            <p
              class="text-xs opacity-60 text-center whitespace-pre-wrap break-words"
            >
              {line.text}
            </p>
          {:else}
            <div
              class="flex {line.role === 'user'
                ? 'justify-end'
                : 'justify-start'}"
            >
              <div
                class="max-w-[80%] whitespace-pre-wrap break-words rounded-base px-3 py-2 text-sm {line.role ===
                'user'
                  ? 'preset-filled-primary-500'
                  : 'preset-tonal-surface'}"
              >
                {line.text}
              </div>
            </div>
          {/if}
        {/each}
        {#if busy}
          <p class="text-xs opacity-60 text-center">working…</p>
        {/if}
        {#if channelState.transcript.status === "error" && channelState.transcript.error}
          <p class="text-xs text-error-500 text-center">
            {channelState.transcript.error}
          </p>
        {/if}
        {#if channelState.status === "offline"}
          <p class="text-xs text-error-500 text-center">
            The tagma is offline. Reopen the channel to reconnect.
          </p>
        {/if}
      </div>
    </div>
    <Composer
      {composer}
      {disabled}
      {busy}
      pendingCount={channelState.pending.length}
      oninterrupt={() => channelsStore.interrupt(convId)}
    />
  </div>
{/if}
