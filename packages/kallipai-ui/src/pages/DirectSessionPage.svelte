<script lang="ts">
  // The direct-session page: /tagma/{tagmaId}/direct/{peer} -- the console's
  // window onto one agent-to-agent 1v1 conversation its tagmas keep on the
  // relay. Rows come from the daemon's structured history read through the
  // open channel's manage bridge (see directSessions.svelte.ts): a paged tail
  // hydrate (25/page from the top, bounded at 20 pages -- transcripts beyond
  // that are a candidate-pool tail-probe) then an incremental poll every 15s
  // keyed on the last seq. Read-only in v1: sending is the agent's voice;
  // an operator-facing send path would need its own trust review. The chat-domain
  // chrome (back row to the chats hub) derives from the trail table, same as
  // /chat/{id}.
  import type { FileAttachment } from "@kallipai/kallipai-lesche-client";
  import type { DirectMessageRow } from "@kallipai/kallipai-client";
  import type { ConversationLine } from "../lib/transcript.ts";
  import { untrack } from "svelte";
  import ConversationView from "../components/ConversationView.svelte";
  import { filesClientOrFail } from "../lib/session/archeion.svelte.ts";
  import { directSessionsStore } from "../lib/session/directSessions.svelte";
  import { createDirectTranscript } from "../lib/session/directTranscript.svelte.ts";
  import { saveBlob } from "../lib/saveBlob.ts";
  import { directQuoteBlocksByTarget } from "../lib/quote.ts";
  import {
    chat_direct_empty,
    chat_direct_title,
    chat_direct_unavailable,
    chat_opening,
    common_retry,
  } from "../paraglide/messages.js";

  let { tagmaId, peerId }: { tagmaId: string; peerId: string } = $props();

  // The transcript lives in a controller: this page instance is reused when
  // the route pair changes, so an answer that arrives after the reader moved
  // on is dropped by generation instead of being written into the new pair.
  const transcript = createDirectTranscript({
    fetchPage: (tagma, peer, afterSeq, limit) =>
      directSessionsStore.fetchTranscript(tagma, peer, afterSeq, limit),
  });
  const phase = $derived(transcript.phase);
  const lines = $derived(
    transcript.rows
      .map(toLine)
      .filter((l): l is ConversationLine => l !== null),
  );

  const peerLabel = $derived(directSessionsStore.peerLabel(peerId));

  // The quote blocks the transcript draws, keyed by the seq of the row each
  // one quotes -- the key the bubble looks up. One pass over the window it
  // holds: the quoting row and its target come from the same read, so a
  // target outside the window stays an absent block instead of a probe.
  const quotes = $derived(directQuoteBlocksByTarget(lines));

  /** Map a wire row onto the shared transcript line. Own messages (sent by
   * the fetch-through daemon's agent) render on the user side and keep the
   * handle the wire stamped on them, so a quoted own row still names an
   * author; the peer's handle displays through the store's label resolution
   * so an enrolled tagma shows the owner's name for it, not the raw stamp. */
  function toLine(row: DirectMessageRow): ConversationLine | null {
    if (row.text === "" && row.attachment === undefined) return null;
    const own = row.sender.tagma_id === tagmaId;
    return {
      historyId: row.seq,
      role: own ? "user" : "assistant",
      text: row.text,
      sender: {
        kind: own ? "user" : "agent",
        id: row.sender.id,
        handle: own
          ? row.sender.handle
          : directSessionsStore.peerLabel(
              row.sender.tagma_id ?? "",
              row.sender.handle,
            ),
      },
      createdAt: row.created_at,
      ...(row.attachment !== undefined ? { attachment: row.attachment } : {}),
      ...(row.quote !== undefined ? { quote: row.quote } : {}),
    };
  }

  // Hydrate on mount, then poll while mounted. The poll is page-scoped (the
  // hub list has its own slower one): a transcript nobody is looking at costs
  // nothing, and navigating away tears the interval down with the effect.
  // Opening a pair bumps the controller's generation, so the previous pair's
  // answers stop counting the moment the route changes.
  $effect(() => {
    transcript.open(tagmaId, peerId);
    const run = () => {
      // The controller reads its own reactive state (rows, last seq): inside
      // an effect that would make this effect depend on the answer it just
      // asked for. The route pair above is the only real trigger.
      untrack(() => {
        void transcript.refresh();
      });
    };
    run();
    const timer = setInterval(run, 15_000);
    return () => {
      clearInterval(timer);
      transcript.close();
    };
  });

  // The file card's download I/O (the ChannelChatPage pattern): pull the
  // referenced record and hand the bytes to the browser as a named download.
  async function downloadAttachment(attachment: FileAttachment): Promise<void> {
    const bytes = await filesClientOrFail().get(attachment.record_id);
    saveBlob(new Blob([bytes]), attachment.name);
  }
</script>

<svelte:head>
  <title>{chat_direct_title({ name: peerLabel })}</title>
</svelte:head>
<div class="h-full flex flex-col">
  <div class="flex-1 min-h-0 flex flex-col">
    <ConversationView
      {lines}
      status={phase === "loading"
        ? "busy"
        : phase === "unavailable"
          ? "error"
          : "idle"}
      error={phase === "unavailable" ? chat_direct_unavailable() : undefined}
      sessionKey={`${tagmaId}:${peerId}`}
      disabled={true}
      pendingCount={0}
      {downloadAttachment}
      resolveQuote={(quotedSeq) => quotes.get(quotedSeq)}
    >
      {#snippet notice()}
        {#if phase === "loading"}
          <p class="text-sm opacity-60 text-center">{chat_opening()}</p>
        {:else if phase === "unavailable"}
          <div class="flex justify-center py-4">
            <button
              type="button"
              class="btn btn-sm preset-outlined-primary-500 hover:preset-filled-primary-500"
              onclick={() => void transcript.retry()}
            >
              {common_retry()}
            </button>
          </div>
        {:else if lines.length === 0}
          <p class="text-sm opacity-60 text-center">{chat_direct_empty()}</p>
        {/if}
      {/snippet}
    </ConversationView>
  </div>
</div>
