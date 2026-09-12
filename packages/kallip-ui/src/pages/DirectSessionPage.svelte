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
  import type { FileAttachment } from "@kallipai/kallip-lesche-client";
  import type { DirectMessageRow } from "@kallipai/kallip-client";
  import type { ConversationLine } from "../lib/transcript.ts";
  import ConversationView from "../components/ConversationView.svelte";
  import { filesClientOrFail } from "../lib/session/archeion.svelte.ts";
  import { directSessionsStore } from "../lib/session/directSessions.svelte";
  import { saveBlob } from "../lib/saveBlob.ts";
  import {
    chat_direct_empty,
    chat_direct_title,
    chat_direct_unavailable,
    chat_opening,
    common_retry,
  } from "../paraglide/messages.js";

  let { tagmaId, peerId }: { tagmaId: string; peerId: string } = $props();

  type Phase = "loading" | "ready" | "unavailable";
  let phase = $state<Phase>("loading");
  let lines = $state<ConversationLine[]>([]);
  let lastSeq = $state(0);

  const peerLabel = $derived(directSessionsStore.peerLabel(peerId));

  /** Map a wire row onto the shared transcript line. Own messages (sent by
   * the fetch-through daemon's agent) render on the user side; the peer's
   * handle displays through the store's label resolution so an enrolled
   * tagma shows the owner's name for it, not the raw relay stamp. */
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
          ? ""
          : directSessionsStore.peerLabel(
              row.sender.tagma_id ?? "",
              row.sender.handle,
            ),
      },
      createdAt: row.created_at,
      ...(row.attachment !== undefined ? { attachment: row.attachment } : {}),
    };
  }

  /** One transcript refresh: hydrate pages from the top when empty, then an
   * incremental tail pull keyed on the last seq. Throws map to the
   * unavailable phase (the retry re-runs the same pipeline). */
  async function refresh(): Promise<void> {
    if (lines.length === 0) {
      const PAGE = 25;
      const MAX_PAGES = 20;
      let collected: DirectMessageRow[] = [];
      let after = 0;
      for (let i = 0; i < MAX_PAGES; i++) {
        const batch = await directSessionsStore.fetchTranscript(
          tagmaId,
          peerId,
          after,
          PAGE,
        );
        collected = [...collected, ...batch];
        if (batch.length < PAGE) break;
        after = batch[batch.length - 1]!.seq;
      }
      lines = collected
        .map(toLine)
        .filter((l): l is ConversationLine => l !== null);
      lastSeq = collected.reduce((max, r) => Math.max(max, r.seq), 0);
    } else {
      const batch = await directSessionsStore.fetchTranscript(
        tagmaId,
        peerId,
        lastSeq,
      );
      if (batch.length > 0) {
        lines = [
          ...lines,
          ...batch.map(toLine).filter((l): l is ConversationLine => l !== null),
        ];
        lastSeq = batch[batch.length - 1]!.seq;
      }
    }
    phase = "ready";
  }

  // Hydrate on mount, then poll while mounted. The poll is page-scoped (the
  // hub list has its own slower one): a transcript nobody is looking at costs
  // nothing, and navigating away tears the interval down with the effect.
  $effect(() => {
    void tagmaId;
    void peerId;
    phase = "loading";
    lines = [];
    lastSeq = 0;
    const run = () => {
      refresh().catch(() => {
        phase = "unavailable";
      });
    };
    void run();
    const timer = setInterval(run, 15_000);
    return () => clearInterval(timer);
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
    >
      {#snippet notice()}
        {#if phase === "loading"}
          <p class="text-sm opacity-60 text-center">{chat_opening()}</p>
        {:else if phase === "unavailable"}
          <div class="flex justify-center py-4">
            <button
              type="button"
              class="btn btn-sm preset-tonal-surface"
              onclick={() => {
                phase = "loading";
                void refresh().catch(() => {
                  phase = "unavailable";
                });
              }}
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
