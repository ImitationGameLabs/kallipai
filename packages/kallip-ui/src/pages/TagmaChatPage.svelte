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
  import { tagmaDetailsPath } from "../lib/shell/routes.ts";
  import { navigate } from "../lib/shell/port.ts";
  import {
    tagma_chat_not_enrolled,
    chat_channel_error,
    chat_channel_unavailable,
    common_retry,
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

  // Open on mount (idempotent), gated on signed-in + enrolled. Tracks ONLY the
  // gate -- deliberately not `channelState` -- so a status transition (peer
  // snapshot, drain death, error) does not re-fire this and re-KEX. ensureOpen
  // is idempotent and the page mount is the single trigger; retry after a hard
  // error is the terminal row's Retry button or navigate away and back (both
  // re-fire this, ensureOpen tears down the dead conversation and re-KEXes).
  // The open is explicit: a user visit outranks the failure budget's
  // gates (an explicit FAILURE still counts; success clears it).
  $effect(() => {
    if (!archeionSession.user) return;
    if (tagma) void channelsStore.ensureOpen(tagma, { explicit: true });
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
