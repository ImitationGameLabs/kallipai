<script lang="ts">
  // One online conversation over an E2EE tagma channel (the independent online
  // path -- its own lean transcript view, not the offline TranscriptView). The
  // transport is a RelayChannel owned by channelsStore; this page just renders
  // its ChannelTranscript and feeds sends through the reused Composer input.
  import Composer from "../components/Composer.svelte";
  import TagmaStatusHeader from "../components/TagmaStatusHeader.svelte";
  import { createComposer } from "../lib/composer.svelte.ts";
  import { createAutoScroll } from "../lib/transcript.svelte.ts";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { navigate } from "../lib/shell/port.ts";

  let { conversationId }: { conversationId: string } = $props();

  // Resolves to undefined for a deep link to a channel that is not currently
  // open (channelsStore only knows channels opened this session).
  const channelState = $derived(channelsStore.get(conversationId));

  // The composer is created once; its closures re-read the reactive state on
  // each submit/canSubmit call.
  const composer = createComposer({
    send: (text) => channelsStore.send(conversationId, text),
    // Busy is not a gate: `ChannelsStore.send` renders the optimistic line at
    // once and POSTs as soon as the previous POST's ack lands (single-in-flight
    // pump), so a mid-turn prompt is buffered rather than dropped.
    canSubmit: () => channelState?.status === "open",
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
  <!-- No open channel for this conversation yet. Channels auto-connect at boot
       and on presence transitions, so this is normally a brief resolving window
       while the key exchange runs. The conversationId is a server-derived value
       the client does not reverse-resolve, so if auto-connect does not open it
       (bogus id, revoked, or offline tagma) the user needs a way out -- route
       them to the tagmata list rather than guessing or stranding them on a
       bare spinner. -->
  <div class="h-full grid place-items-center p-6">
    <div class="text-center flex flex-col gap-3 max-w-sm">
      <p class="text-sm opacity-80">Opening channel…</p>
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
    <TagmaStatusHeader tagmaId={channelState.tagmaId} />
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
        {#each channelState.transcript.lines as line (line.historyId)}
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
                  : 'preset-tonal-surface'} {line.status === 'sending'
                  ? 'opacity-60'
                  : ''}"
              >
                {line.text}
              </div>
              {#if line.role === "user" && line.status === "sending"}
                <span
                  class="self-center ml-2 text-xs opacity-50 animate-pulse"
                  aria-label="sending">··</span
                >
              {/if}
            </div>
          {/if}
        {/each}
        {#if channelState.transcript.status === "error" && channelState.transcript.error}
          <p class="text-xs text-error-500 text-center">
            {channelState.transcript.error}
          </p>
        {/if}
        {#if channelState.status === "offline"}
          <p class="text-xs text-error-500 text-center">
            The tagma is offline. The channel will reconnect automatically when
            it returns.
          </p>
        {/if}
      </div>
    </div>
    <Composer
      {composer}
      {disabled}
      pendingCount={channelState.pending.length}
    />
  </div>
{/if}
