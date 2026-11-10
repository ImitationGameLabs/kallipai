<script lang="ts">
  // One online tagma, resolved to a dedicated backend and handed to the page
  // through a snippet. Pages that drill into a single tagma (agent detail, an
  // agent's turn history) share this wiring. Unlike OnlineManagePage it
  // deliberately does NOT switch the global stores -- the page must render
  // self-sufficiently from the route's tagmaId, because a deep link carries no
  // guarantee that the manage hub ran first. The placeholder rows mirror
  // OnlineManagePage's channel states; the wiring pattern is shared with it for
  // the same reason.
  import type { Snippet } from "svelte";
  import { channelsStore } from "../../lib/session/channels.svelte.ts";
  import { realtimeStore } from "../../lib/session/realtime.svelte.ts";
  import { OnlineBackend } from "../../lib/manage/backend.ts";
  import { ManageRestClient } from "@kallipai/kallipai-keryx-client";
  import { keryxBaseUrlOrFail } from "../../lib/session/archeion.svelte.ts";
  import { manageChannelStalled } from "../../lib/manage/channelStalled.ts";
  import {
    chat_channel_unavailable,
    common_retry,
    manage_opening,
  } from "../../paraglide/messages.js";

  let {
    tagmaId,
    children,
  }: {
    tagmaId: string;
    children: Snippet<[{ backend: OnlineBackend }]>;
  } = $props();

  const channelState = $derived(channelsStore.getTagmaChannelState(tagmaId));
  const conversationId = $derived(
    channelState.kind === "open" ||
      channelState.kind === "offline" ||
      channelState.kind === "error"
      ? (channelState.conversationId ?? null)
      : null,
  );

  // Can this channel still open on its own? absent to a confirmed-offline
  // peer and unavailable cannot (see channelStalled.ts); without this the
  // placeholder copy below would show forever.
  const stalled = $derived(
    !conversationId &&
      manageChannelStalled(
        channelState,
        realtimeStore.resolved && !realtimeStore.has(tagmaId),
      ),
  );
  let backend = $state<OnlineBackend | null>(null);

  $effect(() => {
    if (!conversationId) {
      backend = null;
      return;
    }
    const conv = channelsStore.get(conversationId);
    if (!conv || conv.kind !== "relay") {
      backend = null;
      return;
    }
    // Reading channelState makes this effect re-run when the drain
    // resurrects the transport; connected guards the un-attached window
    // so the channel chain below cannot dereference a missing relay
    // transport.
    void channelState.kind;
    if (!conv.link.connected) {
      backend = null;
      return;
    }
    try {
      // conv.kind === "relay" narrows to RelayConversation
      const relayConv =
        conv as import("../../lib/session/conversation/relayConversation.svelte.ts").RelayConversation;
      const channel = relayConv.relayTransport.relayChannel;
      backend = new OnlineBackend(
        new ManageRestClient(keryxBaseUrlOrFail()),
        channel.tagmaId,
        undefined,
        channel,
      );
    } catch (e) {
      console.error("[online agent scope] backend wiring failed:", e);
      backend = null;
    }
  });
</script>

{#if stalled}
  <!-- No conversation and none can come without a retry: absent to a
       presence-confirmed-offline peer, or an open-budget failure (mirror of
       the chat page's unavailable row). -->
  <div class="h-full grid place-items-center p-6">
    <div class="text-center flex flex-col gap-3 max-w-sm">
      <p class="text-sm text-error-500 dark:text-error-400">
        {chat_channel_unavailable()}
      </p>
      <button
        type="button"
        class="btn preset-outlined-primary-500 hover:preset-filled-primary-500 self-center"
        onclick={() => channelsStore.retryTagma(tagmaId)}
      >
        {common_retry()}
      </button>
    </div>
  </div>
{:else if !backend}
  <div class="h-full grid place-items-center p-6">
    <div class="text-center flex flex-col gap-3 max-w-sm">
      <p class="text-sm opacity-60">{manage_opening()}</p>
    </div>
  </div>
{:else}
  {@render children({ backend })}
{/if}
