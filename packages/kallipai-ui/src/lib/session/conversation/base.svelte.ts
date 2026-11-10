// The shell the three leaves share: the store back-ref and its identity
// guard, the constructor that assembles the four owners (transcript, link,
// outbox, pager), the outbound send(), and the pull policy both transports
// run.
//
// The split against link: link is the connection (its transport, rebuild
// ladder and carrier state); this file decides what to ask the tagma for and
// when -- catch-up and resume with one repair sweep at a time, the newest
// window probe and open, the reset on a numbering this build cannot place,
// and how a page source's failure reaches the reader. What a conversation is
// lives in conversation/{transcript,outbox,pager}; what one frame means lives
// in frames.ts. The two page-source hooks (probePage, loadOlderPage) stay
// abstract; the four reply and lifecycle hooks default to doing nothing.

import { applySignal, EMPTY_TRANSCRIPT } from "../../transcript.ts";
import { applyReply, type FrameTarget } from "./frames.ts";
import { Transcript } from "./transcript.svelte.ts";
import { Outbox } from "./outbox.svelte.ts";
import { Pager } from "./pager.svelte.ts";
import { Link } from "./link.svelte.ts";
import { contentSeq, WINDOW_PAGE } from "./shared.ts";
import type { ConversationSender } from "../../transcript.ts";
import type {
  HistoryEntry,
  Participant,
  TagmaReply,
} from "@kallipai/kallipai-lesche-client";
import { WriteRefusal } from "../writeRefusal.ts";
import { clearConvCache } from "@kallipai/kallipai-keryx-client";
import type { SendOptions, Transport } from "../transport.ts";
import type {
  HistoryPage,
  MessageSender,
  PullOpts,
} from "../messageSender.svelte.ts";

/** The minimal store surface a Conversation needs (back-ref for the stale
 *  guard). Defined here so this module does not import the store (avoids a
 *  cycle). */
export interface ConversationStoreLike {
  get(id: string): ConversationBase | undefined;
}

export abstract class ConversationBase {
  /** The read model: rendered rows, their positions, the row-id bracket
   *  the pager pages by, and the numbering freeze. */
  readonly transcript: Transcript;
  /** The read path's connection: its transport, status, error, carrier and
   *  restart ladder. */
  readonly link: Link;
  /** Source of synthetic negative ids for lines without a real history_id.
   *  Plain (the {#each} key is read at line construction, not observed). */
  syntheticSeq = 0;
  abstract readonly kind: "local" | "relay" | "offline-view";

  /** The IndexedDB cache key (the tagma's conversation id). The offline
   *  `"local"` entry and the online derived-id entry for the same tagma share
   *  this so a mode switch rehydrates from the same rows. Falls back to the
   *  store key when the tagma is not enrolled (no durable history). */
  readonly cacheConversationId: string;
  /** True while a repair pass runs (single-flight guard). */
  private resuming = false;
  /** Set when a repair is asked for while one runs: the running pass takes
   *  one more sweep instead of stacking pulls. */
  private resumeAgain = false;
  /** The probe in flight: a catch-up joining it shares its answer instead of
   *  asking the tagma the same question again. */
  private probe: Promise<void> | null = null;
  /** The send side: the queue of optimistic lines, the single POST in
   *  flight, the discards, and the reply watchdog. */
  readonly outbox: Outbox;

  /** The pager: scroll-up pages, the no-more-older flag, and the one-row
   *  probe a quote block asks for. */
  readonly pager: Pager;

  /** The reducer's view of this conversation: the read model, the send
   *  queue, and the leaf's answers, fixed once here so no frame builds it
   *  (see conversation/frames.ts). */
  readonly frames: FrameTarget;

  /** Mint the next negative id a locally drawn line carries until the tagma
   *  names it: one counter, one step down per line. */
  private nextSyntheticId(): number {
    return (this.syntheticSeq -= 1);
  }

  constructor(
    readonly conversationId: string,
    protected readonly store: ConversationStoreLike,
    transport: Transport | null,
    cacheConversationId: string,
    /** The UI sender for optimistic user lines (online: the archeion session;
     *  offline: the tagma-configured local identity). */
    protected readonly localSender: ConversationSender,
    /** The write path's carrier; null = no sender (the conversation is closed). */
    sender: MessageSender | null = null,
  ) {
    this.cacheConversationId = cacheConversationId;
    this.transcript = new Transcript(cacheConversationId, (cursor) =>
      this.reportPosition(cursor),
    );
    this.link = new Link(
      () => this.isLive(),
      () => this.carrierLive,
      (reply, sender) => this.applyReplyCore(reply, sender),
      (signal) => {
        this.transcript.state = applySignal(
          this.transcript.state,
          signal,
          this.nextSyntheticId(),
        );
      },
      () => this.onStreamAttached(),
      () => this.onDrainDead(),
    );
    this.link.transport = transport;
    this.link.sender = sender;
    this.outbox = new Outbox(
      cacheConversationId,
      this.transcript,
      localSender,
      () => this.pendingKey,
      () => this.link.sender,
    );
    this.pager = new Pager(
      this.transcript,
      (k) => this.loadOlderPage(k),
      (id) => this.probePage(id),
    );
    this.frames = {
      transcript: this.transcript,
      outbox: this.outbox,
      cacheConversationId,
      pendingPartition: () => this.pendingKey,
      nextSyntheticId: () => this.nextSyntheticId(),
      repair: () => void this.resume(),
      after: (reply) => this.onReply(reply),
    };
  }

  /** The leaf's share of the write axis: does the path a line travels exist
   *  right now. The direct leaf's carrier outlives its stream, so this is a
   *  mounted sender that has not closed; the relay leaf's path is its own
   *  channel; the offline view writes into the pending store. `available` says
   *  'this sender was not closed', never 'a send can go through now'. */
  protected get carrierLive(): boolean {
    return this.link.sender !== null && this.link.sender.available;
  }

  /** Send a user message online: renders the optimistic line, queues it,
   *  and hands off to the shared single-in-flight send pump (the in-flight
   *  POST's `user_message` frame promotes the line via `applyReplyCore`). */
  send(text: string, options?: SendOptions): void {
    if (!this.link.canWrite) {
      this.link.error = new WriteRefusal(
        this.link.closed ? "closed" : "carrier-absent",
      );
      return;
    }
    const trimmed = text.trim();
    if (trimmed === "" && options?.attachment === undefined) return;
    this.outbox.enqueue(trimmed, options);
  }

  /** The pending-store partition this conversation's unsent lines live
   *  under (the tagma id; the offline shell's local conversation uses the
   *  literal "local", mirroring its cache-key fallback). */
  abstract get pendingKey(): string;

  /** Hook for leaves to react to a reply AFTER the shared core reduce (the relay
   *  flips `live` on `history_batch_end` and fires background notifications). */
  protected onReply(_reply: TagmaReply): void {}

  /** Reduce one authored reply through the shared core (frames.ts): dedup
   *  by `history_id`, close or promote the in-flight optimistic line, adopt
   *  a replay, cache the row, and step the cursor. `sender` is the wire
   *  participant who authored the content. Leaves wrap this (the relay
   *  reroutes replay rows into its pending pages), and the drain guards it
   *  on `isLive`, so a stale stream cannot touch a fresher entry. */
  protected applyReplyCore(
    reply: TagmaReply,
    sender: Participant | undefined,
  ): void {
    applyReply(this.frames, reply, sender);
  }

  /** Report a read position to the tagma. The relay carries the write (its
   *  manage bridge PUTs the cursor); the direct path has no bridge and leaves
   *  the badge to the tagma's own unread frames, so it reports nothing. The
   *  base hook keeps one call site for both leaves. */
  protected reportPosition(_cursor: number): void {}

  /** Catch up: repair the log above the resume cursor, or open the newest
   *  window when this device holds no position yet. Live-safe and idempotent
   *  (pulled rows merge by row id), so every entry point -- a fresh mount, a
   *  resumed stream, a repair marker -- calls this and the worst case is a
   *  redundant pull. */
  async catchUp(k = WINDOW_PAGE): Promise<void> {
    await this.resume(k);
  }

  /** Repair the log above the cursor: page `after_seq` forward from the
   *  request anchor until the tagma reports nothing more, folding each page
   *  as it lands. One repair at a time: a request arriving mid-flight asks
   *  the running pass for one more sweep instead of stacking pulls, so a
   *  burst of frames costs one sweep.
   *
   *  Two numbers drive the loop and they are not the same one. The anchor
   *  says where the next request starts; it follows `next`, the page's own
   *  edge over raw rows, so a tail this device could not decode is asked for
   *  once instead of on every frame and every reconnect. The cursor says how
   *  far this device has rendered, and it moves only over rows that landed:
   *  a page carrying more rows than could be placed leaves it where it was.
   *  Nothing rendered through a whole page means the rows above the cursor
   *  are out of reach here (retention removed them, or the decoder cannot
   *  read them): the cursor steps over that block once, with the block named
   *  in the warning, instead of re-reading it forever. A `next` that names no
   *  new position ends the sweep. */
  async resume(k = WINDOW_PAGE): Promise<void> {
    const sender = this.link.sender;
    const pull = sender?.pullHistory;
    if (pull === undefined) return;
    // A frozen window pulls nothing: every page would carry positions this
    // build cannot place, and a reload is the way out.
    if (this.transcript.frozen) return;
    if (this.resuming) {
      this.resumeAgain = true;
      return;
    }
    this.resuming = true;
    try {
      do {
        this.resumeAgain = false;
        await this.repairOnce(pull.bind(sender), k);
      } while (this.resumeAgain && !this.link.closed);
    } finally {
      this.resuming = false;
    }
  }

  /** One repair sweep: {@link resume}'s body, split out so the single-flight
   *  loop reads as the loop it is. */
  private async repairOnce(
    pull: (opts: PullOpts) => Promise<HistoryPage>,
    k: number,
  ): Promise<void> {
    if (this.transcript.frozen) return;
    let cursor = this.transcript.lastSeq;
    let anchor = this.transcript.lastSeq;
    try {
      while (!this.link.closed) {
        cursor = this.transcript.lastSeq;
        anchor = Math.max(anchor, cursor);
        if (cursor <= 0) {
          await this.openNewest(pull, k);
          return;
        }
        const page = await pull({ afterSeq: anchor, limit: k });
        this.transcript.historyError = null;
        if (
          page.rows.length === 0 &&
          !page.more &&
          !page.undecodable &&
          cursor > 0
        ) {
          // An empty page that also ends the sweep answers one of two
          // things and cannot say which: this position is the live tail, or
          // rows were numbered differently since it was written. The newest
          // page answers it without a cursor to walk from.
          // Every such page asks, however recently one answered: a
          // remembered tail saves one request and risks a verdict that a
          // renumbering makes wrong for the rest of the page's life.
          // A page with rows that did not decode is not this case: it proves
          // rows exist above the cursor, and the step-over below moves past
          // them instead.
          await this.probeNewest(pull, cursor, k);
          return;
        }
        const first = page.rows
          .map((r) => contentSeq(r.reply))
          .find((s): s is number => s !== null);
        if (first !== undefined && first > cursor + 1) {
          // Two facts and no verdict: how wide the gap is, and how many rows
          // of this page the decoder could not read. Either cause alone would
          // explain the gap, and one page cannot say which one it was.
          console.warn(
            "[chat] " +
              (first - cursor - 1) +
              " row(s) between " +
              cursor +
              " and " +
              first +
              " are missing here; " +
              page.undecodable +
              " row(s) in this page did not decode; continuing from " +
              first,
          );
          this.transcript.lastSeq = first - 1;
        }
        if (page.rows.length > 0) this.transcript.applyPulled(page.rows);
        // `more` answers what lies above the page and `next` where the same
        // request continues. The anchor follows `next`; the cursor follows
        // rendered rows. A page whose tail did not decode leaves the cursor
        // behind its edge: that block is stepped over once, with the block
        // named, so the sweep does not ask for it again on every frame. The
        // step happens even when the block runs to the newest row, where no
        // page above it exists to carry the cursor forward.
        const edge = page.next;
        if (edge === null || edge <= anchor) return;
        if (this.transcript.lastSeq < edge && page.undecodable > 0) {
          console.warn(
            "[chat] stepping from " +
              this.transcript.lastSeq +
              " to " +
              edge +
              "; " +
              page.undecodable +
              " row(s) in this page did not decode, so part of that span" +
              " is not on screen",
          );
          this.transcript.lastSeq = edge;
        }
        if (!page.more) return;
        anchor = edge;
      }
    } catch (e) {
      // A failed pull is not a filled hole: the cursor stays where it was, so
      // the next frame or the next mount asks again.
      console.warn("[chat] resume failed above " + cursor + ":", e);
      this.transcript.notePullFailure(e);
    }
  }

  /** Ask the newest page what the tagma actually holds, then judge: nothing
   *  at all or an older numbering (either way the window resets), and
   *  otherwise the empty page was the live tail. A catch-up arriving
   *  mid-probe waits for the answer instead of asking it again. */
  private async probeNewest(
    pull: (opts: PullOpts) => Promise<HistoryPage>,
    cursor: number,
    k: number,
  ): Promise<void> {
    if (this.probe !== null) {
      await this.probe;
      return;
    }
    const probe = (async () => {
      const page = await pull({ limit: 1 });
      const newest = page.next;
      // No history at all, or rows numbered lower than this cursor: either
      // way the position is not a place on the tagma's log. Anything else
      // means the empty page was the live tail.
      if (newest === null || newest < cursor) await this.resetWindow(pull, k);
    })();
    this.probe = probe;
    try {
      await probe;
    } finally {
      if (this.probe === probe) this.probe = null;
    }
  }

  /** Drop the position and the rows it ordered, then open the newest window
   *  again. Moving the cursor alone would leave old-numbered rows on screen,
   *  and the next scroll-up would page by ids the tagma has since given to
   *  other rows. Lines still owed a send stay, the one the pump holds in
   *  flight included: each exists nowhere else. */
  private async resetWindow(
    pull: (opts: PullOpts) => Promise<HistoryPage>,
    k: number,
  ): Promise<void> {
    await clearConvCache(this.cacheConversationId);
    const unsent = this.transcript.state.lines.filter(
      (line) =>
        this.outbox.pending.some((entry) => entry.localId === line.historyId) ||
        line.historyId === this.outbox.pendingInFlight?.localId,
    );
    this.transcript.state = { ...EMPTY_TRANSCRIPT, lines: unsent };
    this.transcript.lastSeq = 0;
    this.transcript.minSeq = 0;
    this.transcript.minRowId = 0;
    this.transcript.maxRowId = 0;
    this.pager.hasMoreOlder = true;
    await this.openNewest(pull, k);
  }

  /** Open the newest window: the cold-start read, when there is no cursor to
   *  continue from. Its newest rendered position becomes the cursor -- a
   *  report of zero would claim nothing was ever read, leaving the tagma to
   *  count rows this device already shows. */
  protected async openNewest(
    pull: (opts: PullOpts) => Promise<HistoryPage>,
    k: number,
  ): Promise<void> {
    if (this.transcript.frozen) return;
    try {
      const page = await pull({ limit: k });
      this.transcript.historyError = null;
      if (page.rows.length > 0) this.transcript.applyPulled(page.rows);
      // A window short of k raw rows IS all the history there is; one that
      // arrived complete proves nothing about what lies below it.
      if (page.rows.length + page.undecodable < k)
        this.pager.hasMoreOlder = false;
    } catch (e) {
      // Dead carrier: the drain surfaces the status, the window stays as it
      // is, and live frames keep arriving; say so anyway, because a cold
      // start that quietly shows nothing looks like an empty log.
      console.warn("[chat] could not open the newest window:", e);
      this.transcript.notePullFailure(e);
    }
  }

  /** The leaf's page for one probe: what the leaf answers for the row id
   *  `id`, which the caller reads as proof of presence or absence. The direct
   *  leaf asks the tagma for that one row by id; the relay asks its control
   *  channel for the row just before `id + 1`, which names the same single
   *  row. Either way the answer is about the id itself, and that is why
   *  `peekRow` may read an empty page as proof of absence -- the row count of
   *  a position window is not what makes the verdict sound.
   *
   *  Read and not merged -- or `null` when this leaf cannot give a COMPLETE
   *  page right now (no history source, a stream that is not open, a catch-up
   *  holding the frames, a read that failed, a window the tagma could not
   *  decode whole, a page that arrived short of its own count). `null` reads
   *  as a retryable `error`, never as a verdict. */
  protected abstract probePage(id: number): Promise<HistoryEntry[] | null>;

  /** The leaf page source (cache-first, then transport). The caller
   * holds the single-flight slot and has verified the window has
   * formed. */
  protected abstract loadOlderPage(k: number): Promise<void>;

  /** Retry one failed line. The queue mechanics live in Outbox; whether a
   *  retry exists at all is the leaf's call (the offline view has nothing to
   *  retry into, so it keeps the line as it is). */
  retrySend(localSeq: number): void {
    this.outbox.retrySend(localSeq);
  }

  /** True iff this conversation is still the store's live entry for its id. */
  protected isLive(): boolean {
    return this.store.get(this.conversationId) === this;
  }

  /** Hook: a transport now drives the read path (the first attach and every
   *  rebuild). Subclasses wire their stream lifecycle here. */
  protected onStreamAttached(): void {}

  /** Hook for relay-only cleanup when the drain dies (abandon pending sends). */
  protected onDrainDead(): void {}

  /** Start the read path (the first attach and every reattach) and resolve
   *  when the drain ends: callers await it to know the transport is gone. */
  run(): Promise<void> {
    return this.link.run();
  }

  /** Close the conversation: the drain ends, the carrier closes and the
   *  restart wait stops here. */
  close(): void {
    this.link.close();
  }
}
