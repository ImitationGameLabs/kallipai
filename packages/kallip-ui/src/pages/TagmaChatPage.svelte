<script lang="ts">
  // The tagma-keyed chat page: /tagma/{tagmaId}/chat. Unlike
  // /chat/{conversationId}
  // (where the id is server-derived and only known AFTER a relay channel is
  // open), this route is always navigable for an enrolled tagma -- the channel
  // opens on demand here, mirroring RoomConversationPage's on-mount open. Once
  // a conversation exists, the body delegates to ChannelChatPage (status
  // header + transcript + composer + the offline/error chrome it already
  // owns), so this page is a thin resolver + opener.

  import ChannelChatPage from "./ChannelChatPage.svelte";
  import { archeionSession } from "../lib/session/archeion.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { realtimeStore } from "../lib/session/realtime.svelte";
  import { tagmaDetailsPath } from "../lib/shell/routes.ts";
  import { navigate } from "../lib/shell/port.ts";
  import {
    tagma_chat_not_enrolled,
    chat_channel_error,
    chat_channel_unavailable,
    common_retry,
    chat_offline_no_history,
    chat_opening,
    chat_go_tagmata,
    nav_breadcrumb_tagma,
    nav_chat,
  } from "../paraglide/messages.js";

  let { tagmaId }: { tagmaId: string } = $props();

  // Resolve the tagma from the registry (presence-independent -- the registry
  // is the always-show source for the sidebar, and the only source that can
  // confirm the id is still enrolled).
  const tagma = $derived(
    archeionSession.tagmata.find(
      (t) => t.tagma_id === tagmaId && t.state === "enrolled",
    ),
  );

  // Channel transport state, for render branching only. NOT read inside the
  // ensureOpen effect below (see the comment there).
  const channelState = $derived(channelsStore.getTagmaChannelState(tagmaId));

  // The conversationId once the channel is settled (open/offline/error);
  // absent/pending have none and render the opening placeholder.
  const conversationId = $derived(
    channelState.kind === "open" ||
      channelState.kind === "offline" ||
      channelState.kind === "error"
      ? channelState.conversationId
      : null,
  );

  // The mounted degraded offline view (peer reads offline, or the open
  // failed): history from the device cache when any, the empty-history
  // notice otherwise; sends land in the pending store.
  const offlineView = $derived(channelsStore.offlineViewOf(tagmaId));

  // Open on mount for an online peer, gated on signed-in + enrolled -- and
  // NEVER for a peer whose presence reads offline: an offline peer gets
  // the offline history view, not a fresh open attempt. A mount open
  // would have to bypass the failure budget to be explicit, and an
  // effect re-fire is not user intent -- so online peers open through
  // the AUTOMATIC path, where the budget's backoff and terminal gate
  // apply; only the Retry button is explicit.
  // The effect tracks ONLY its gates (user presence/enrollment/offline) --
  // not channelState -- so status transitions do not re-fire it. The
  // offline branch mounts the degraded history view (cached tail, or an
  // empty one when this device has no local history).
  const offlinePeer = $derived(
    realtimeStore.resolved && !realtimeStore.has(tagmaId),
  );
  $effect(() => {
    if (!archeionSession.user) return;
    if (offlinePeer) {
      void channelsStore.attachOfflineView(tagmaId);
      return;
    }
    if (tagma) void channelsStore.ensureOpen(tagma);
  });
</script>

<div class="h-full flex flex-col">
  <div class="flex-1 min-h-0 flex flex-col">
    {#if !tagma}
      <!-- Not enrolled / revoked / unknown id. The registry is authoritative; a
       revoked tagma falls here on the next refresh. -->
      <div class="h-full grid place-items-center p-6">
        <div class="text-center flex flex-col gap-3 max-w-sm">
          <p class="text-sm opacity-80">{tagma_chat_not_enrolled()}</p>
          <button
            type="button"
            class="btn preset-outlined-surface-500 hover:preset-filled-surface-500 self-center"
            onclick={() => navigate("/tagmata")}
          >
            {chat_go_tagmata()}
          </button>
        </div>
      </div>
    {:else if conversationId}
      {#if channelState.kind === "error"}
        <!-- The channel died after opening (transport error). The transcript
         stays readable below; this row carries the user-facing retry. -->
        <div
          class="border-b border-surface-200-800 px-4 py-2 flex items-center justify-center gap-3"
        >
          <p class="text-xs text-error-500 dark:text-error-400">
            {chat_channel_error()}
          </p>
          <button
            type="button"
            class="btn btn-sm preset-outlined-primary-500 hover:preset-filled-primary-500"
            onclick={() => channelsStore.retryTagma(tagmaId)}
          >
            {common_retry()}
          </button>
        </div>
      {/if}
      <div class="flex-1 min-h-0 flex flex-col">
        <div class="flex-1 min-h-0">
          <ChannelChatPage {conversationId} />
        </div>
      </div>
    {:else if offlineView}
      <!-- Degraded offline view: cached transcript, retryable sends. The
       retry row re-arms the real channel; sends land in the pending store
       and auto-flush when the connection comes back. -->
      <div class="flex-1 min-h-0 flex flex-col">
        <div
          class="border-b border-surface-200-800 px-4 py-2 flex items-center justify-center gap-3"
        >
          <p class="text-xs text-error-500 dark:text-error-400">
            {chat_channel_unavailable()}
          </p>
          <button
            type="button"
            class="btn btn-sm preset-outlined-primary-500 hover:preset-filled-primary-500"
            onclick={() => channelsStore.retryTagma(tagmaId)}
          >
            {common_retry()}
          </button>
        </div>
        <div class="flex-1 min-h-0">
          {#if offlineView.transcript.lines.length === 0}
            <div class="h-full grid place-items-center p-6">
              <p class="text-sm opacity-70 max-w-sm text-center">
                {chat_offline_no_history()}
              </p>
            </div>
          {:else}
            <ChannelChatPage conversationId={offlineView.conversationId} />
          {/if}
        </div>
      </div>
    {:else if channelsStore.isAutoOpenFailed(tagmaId) && channelState.kind !== "pending"}
      <!-- The last open attempt failed (budget entry) and none is in flight;
       without this branch the opening placeholder would spin forever.
       Retry re-arms and opens explicitly. -->
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
    {:else}
      <!-- absent / pending: ensureOpen has been fired by the effect above. -->
      <div class="h-full grid place-items-center p-6">
        <div class="text-center flex flex-col gap-3 max-w-sm">
          <p class="text-sm opacity-60">{chat_opening()}</p>
          <button
            type="button"
            class="btn preset-outlined-surface-500 hover:preset-filled-surface-500 self-center"
            onclick={() => navigate("/tagmata")}
          >
            {chat_go_tagmata()}
          </button>
        </div>
      </div>
    {/if}
  </div>
</div>
