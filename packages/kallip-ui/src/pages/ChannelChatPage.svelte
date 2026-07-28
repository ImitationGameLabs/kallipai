<script lang="ts">
  // One conversation over a transport -- relayed (online, E2EE RelayChannel) or
  // local (offline, DirectTransport). The store resolves `conversationId` to a
  // Conversation (`"local"` for offline, a server-derived id for online); the
  // page owns the chrome (status header + a local-aware empty state) and
  // composes the shared <ConversationView> for the transcript body. The page is
  // fully mode-agnostic: status, transcript, and pending count are all read off
  // the Conversation, so online and offline render identically.
  import ConversationView from "../components/ConversationView.svelte";
  import TagmaStatusHeader from "../components/TagmaStatusHeader.svelte";
  import { createComposer } from "../lib/composer.svelte.ts";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { navigate } from "../lib/shell/port.ts";

  let { conversationId }: { conversationId: string } = $props();

  // Resolves to undefined only briefly: online while a channel's key exchange
  // runs, or offline on a /chat/local deep-link before the boot reconnect lands
  // (the gate routes a failed reconnect to /connect, so this is a short window).
  const conv = $derived(channelsStore.get(conversationId));
  const isLocal = $derived(conversationId === "local");

  const composer = createComposer({
    send: (text) => channelsStore.send(conversationId, text),
    // Busy is not a gate: send renders the optimistic line at once and POSTs as
    // soon as the previous POST's user_message frame lands (single-in-flight
    // pump, shared by both transports).
    canSubmit: () => conv?.status === "open",
  });

  const disabled = $derived(!conv || conv.status !== "open");
  const pendingCount = $derived(conv?.pending.length ?? 0);
</script>

<svelte:head
  ><title>KallipAI · {isLocal ? "chat" : "channel"}</title></svelte:head
>

{#if !conv}
  {#if isLocal}
    <!-- Offline /chat/local before the boot reconnect lands. The gate routes a
         failed reconnect to /connect; this is the brief resolving window. -->
    <div class="h-full grid place-items-center p-6">
      <p class="text-sm opacity-60">Connecting…</p>
    </div>
  {:else}
    <!-- No open channel for this conversation yet. Channels auto-connect at
         boot and on presence transitions, so this is normally brief. The
         conversationId is server-derived and not reverse-resolvable, so if
         auto-connect does not open it (bogus id, revoked, offline tagma) the
         user needs a way out. -->
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
  {/if}
{:else}
  <div class="flex flex-col h-full">
    <TagmaStatusHeader status={conv.statusSnapshot} />
    <ConversationView
      lines={conv.transcript.lines}
      status={conv.transcript.status}
      error={conv.transcript.error}
      {composer}
      {disabled}
      {pendingCount}
    >
      {#snippet notice()}
        {#if conv.status === "offline"}
          <p class="text-xs text-error-500 text-center">
            {#if isLocal}
              The tagma connection dropped. Reconnect from settings.
            {:else}
              The tagma is offline. The channel will reconnect automatically
              when it returns.
            {/if}
          </p>
        {/if}
      {/snippet}
    </ConversationView>
  </div>
{/if}
