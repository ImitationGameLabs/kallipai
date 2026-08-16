<script lang="ts">
  // The tagma-keyed chat page: /chat/t/{tagmaId}. Unlike /chat/{conversationId}
  // (where the id is server-derived and only known AFTER a relay channel is
  // open), this route is always navigable for an enrolled tagma -- the channel
  // opens on demand here, mirroring RoomConversationPage's on-mount open. Once
  // a conversation exists, the body delegates to ChannelChatPage (status
  // header + transcript + composer + the offline/error chrome it already
  // owns), so this page is a thin resolver + opener.

  import ChannelChatPage from "./ChannelChatPage.svelte";
  import { agoraSession } from "../lib/session/agora.svelte";
  import { channelsStore } from "../lib/session/channels.svelte";
  import { navigate } from "../lib/shell/port.ts";
  import {
    tagma_chat_not_enrolled,
    chat_opening,
    chat_go_tagmata,
  } from "../paraglide/messages.js";

  let { tagmaId }: { tagmaId: string } = $props();

  // Resolve the tagma from the registry (presence-independent -- the registry
  // is the always-show source for the sidebar, and the only source that can
  // confirm the id is still enrolled).
  const tagma = $derived(
    agoraSession.tagmata.find(
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
  // error is "navigate away and back" (mount re-fires this, ensureOpen tears
  // down the dead conversation and re-KEXes).
  $effect(() => {
    if (!agoraSession.user) return;
    if (tagma) void channelsStore.ensureOpen(tagma);
  });
</script>

{#if !tagma}
  <!-- Not enrolled / revoked / unknown id. The registry is authoritative; a
       revoked tagma falls here on the next refresh. -->
  <div class="h-full grid place-items-center p-6">
    <div class="text-center flex flex-col gap-3 max-w-sm">
      <p class="text-sm opacity-80">{tagma_chat_not_enrolled()}</p>
      <button
        type="button"
        class="btn preset-tonal-surface self-center"
        onclick={() => navigate("/tagmata")}
      >
        {chat_go_tagmata()}
      </button>
    </div>
  </div>
{:else if conversationId}
  <div class="h-full flex flex-col">
    <div class="flex-1 min-h-0">
      <ChannelChatPage {conversationId} />
    </div>
  </div>
{:else}
  <!-- absent / pending: ensureOpen has been fired by the effect above. -->
  <div class="h-full grid place-items-center p-6">
    <div class="text-center flex flex-col gap-3 max-w-sm">
      <p class="text-sm opacity-60">{chat_opening()}</p>
      <button
        type="button"
        class="btn preset-tonal-surface self-center"
        onclick={() => navigate("/tagmata")}
      >
        {chat_go_tagmata()}
      </button>
    </div>
  </div>
{/if}
