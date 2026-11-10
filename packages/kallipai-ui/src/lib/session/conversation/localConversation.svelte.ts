// The offline leaf: a plain forwarder over the direct SSE. Its stream carries
// the frames, its sender carries the writes, and its page source reads the
// cache first and the transport second.

import { cachedLineToLine } from "./shared.ts";
import type { HistoryEntry } from "@kallipai/kallipai-lesche-client";
import { readTailBefore } from "@kallipai/kallipai-keryx-client";
import type { Transport } from "../transport.ts";
import type {
  HistoryPage,
  MessageSender,
  PullOpts,
} from "../messageSender.svelte.ts";
import { DirectTransport, type TransportState } from "../directTransport.ts";
import { ConversationBase, type ConversationStoreLike } from "./base.svelte.ts";

export class LocalConversation extends ConversationBase {
  readonly kind = "local" as const;
  get pendingKey(): string {
    return "local";
  }

  constructor(
    store: ConversationStoreLike,
    transport: Transport,
    cacheConversationId: string,
    /** The write path's carrier; the reattach path keeps it while swapping streams. */
    sender: MessageSender | null = null,
    /** Rebuilds the read-path transport after its drain ended. */
    restartStream: (() => Transport) | null = null,
  ) {
    // attachLocal resolves the transport before binding; there is no opening
    // window. The conversation is open for the moment the drain runs; it flips
    // to offline/error when the SSE ends or fails.
    super(
      "local",
      store,
      transport,
      cacheConversationId,
      transport.localSender,
      sender,
    );
    this.link.status = "open";
    this.link.restartStream = restartStream;
    this.onStreamAttached();
  }

  /** The direct history pull, taken from the sender: a dead stream cannot take
   *  the backfill with it. Null when this conversation has no puller. */
  private get pull(): ((opts: PullOpts) => Promise<HistoryPage>) | null {
    const sender = this.link.sender;
    const pull = sender?.pullHistory;
    return pull === undefined ? null : pull.bind(sender);
  }

  /** Wire the stream's retry lifecycle for whichever transport drives the read
   *  path right now -- the first attach and every rebuild. A `missed` frame
   *  carries no position: this device's own cursor says where to resume, so
   *  the repair starts there. */
  protected override onStreamAttached(): void {
    const t = this.link.transport;
    if (t instanceof DirectTransport) {
      t.onState = (s) => this.onTransportState(s);
      t.onMissed = () => {
        void this.resume();
      };
    }
  }

  /** Stream lifecycle from the direct transport's retry loop (wired by
   *  onStreamAttached): "reconnecting" flips the status -- the chat page shows
   *  an in-chat spinner and the channel lamp goes dark, while the composer
   *  keeps writing (canWrite does not read the stream); "resumed" restores
   *  "open" and repairs whatever frames the reconnect dropped (resume is
   *  idempotent and live-safe). */
  onTransportState(s: TransportState): void {
    if (!this.isLive()) return;
    if (s === "reconnecting") {
      this.link.status = "reconnecting";
    } else {
      this.link.status = "open";
      this.link.resetRestartBackoff();
      // Back online: flush the durable unsent lines once (retrySend is
      // idempotent, so racing retries collapse).
      void this.outbox.retryAllPending();
      void this.resume();
    }
  }
  /** The local leaf's page source. Cache-first: a full cache page never
   *  touches the tagma (the offline degrade path — the cache holds everything
   *  ever rendered); a short cache page falls through to the server for the
   *  remainder. A zero-add server page disarms the sentinel (the oldest
   *  reachable row is already in the window). Guards, single-flight, and
   *  quiet error handling live in the base loadOlder. */
  protected override async loadOlderPage(k: number): Promise<void> {
    const head = this.transcript.minRowId;
    const below = this.transcript.minSeq;
    const cached = await readTailBefore(this.cacheConversationId, head, k);
    if (cached.length > 0) {
      this.transcript.merge(cached.map(cachedLineToLine));
    }
    if (cached.length >= k) return;
    const pull = this.pull;
    if (pull === null) return;
    // Below the window means below its oldest rendered position: a window
    // read takes a position, and a page's `next` is its own edge, not the
    // anchor the cache just extended.
    if (below <= 0) return;
    const { rows, more } = await pull({ beforeSeq: below, limit: k });
    const added = this.transcript.applyPulled(rows);
    // Same distinction as the relay leaf: !more reads the database, while
    // added === 0 reads this window -- all-dropped rows cannot move the cursor.
    if (!more || added === 0) this.pager.hasMoreOlder = false;
  }

  /** The local leaf's probe: one row, asked for by id, read straight off the
   *  transport and not merged -- the window keeps its floor, so a mid-history
   *  segment cannot leave a hole. An id names that row on the server, so the
   *  answer does not depend on what this device holds: whether the row is
   *  there, and what it says, come back as one entry or as an empty page.
   *  `undecodable` still reads as a hole instead of an absence: a row that
   *  exists but cannot be read proves nothing about what the quote points at. */
  protected async probePage(id: number): Promise<HistoryEntry[] | null> {
    const pull = this.pull;
    // No probe while the stream is down or reconnecting.
    if (pull === null || this.link.status !== "open") return null;
    try {
      const { rows, undecodable } = await pull({ row: id, limit: 1 });
      // A hole cannot prove a contiguous miss; the reply is one atomic JSON
      // body, so the only rows it can lose are the ones dropped in decode.
      return undecodable > 0 ? null : rows;
    } catch {
      return null;
    }
  }
}
