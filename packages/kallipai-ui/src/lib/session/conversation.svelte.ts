// Per-conversation reactive state, drained from a {@link Transport}. One base
// holds the shared surface (transcript, transport status, the two-stream drain,
// the single-in-flight send pump, the history cursor + cache + dedup, and the
// optimistic-line promotion), and the relay leaf carries its transport-specific
// extras (req_id-correlated cursor pulls, the before-page reroute, and the
// background-notification gate):
//
//   - `RelayConversation` (online): the E2EE pipe; history pages are cursor
//     pulls that await their batch_end marker. A catch-up ADOPTS the newest
//     window as one batch (one fold) instead of
//     replaying the whole gap row by row; scroll-up pages continue below the
//     window head, and replayed rows below the rendered cursor reroute into
//     the pending page. Only frames above the open-time high-water fire
//     background notifications.
//   - `LocalConversation` (offline): a plain forwarder over the direct SSE.
//
// The two leaves share the `applyReplyCore` reducer path (dedup by
// `history_id`, cache, `user_message` promotion of the optimistic line —
// the relay leaf wraps it with the page reroute) and the run() status
// transitions verbatim. Every drain mutation is guarded by an object-identity
// check against the store's live entry for this id (a reconnect replaces the
// entry under the same id; a stale drain must not touch the fresh one).
//
// The offline (store key `"local"`) and online (store key = derived id) entries
// for the SAME tagma share one IndexedDB cache via `cacheConversationId` (the
// tagma's conversation id), so a mode switch rehydrates from the same rows.

import {
  applySignal,
  applyTagmaReply,
  cacheLineOf,
  dropFailedLine,
  EMPTY_TRANSCRIPT,
  historyEntryLine,
  isInFlightError,
  markLineSent,
  mergeHistoryLines,
  nextPendingSeq,
  replaceLineId,
  retryLine,
  sendFailed,
  stampPosition,
  toSender,
  withUserLine,
} from "../transcript.ts";
import type {
  ConversationLine,
  ConversationSender,
  ConversationTranscript,
} from "../transcript.ts";
import type {
  FileAttachment,
  HistoryEntry,
  Participant,
  TagmaReply,
} from "@kallipai/kallipai-lesche-client";
import type { CachedLine, RelayChannel } from "@kallipai/kallipai-keryx-client";
import { manageOk } from "@kallipai/kallipai-keryx-client";
import { relayRootAgentId } from "./rootAgent.ts";
import {
  clearConvCache,
  deletePending,
  isFailedPending,
  markPendingFailed,
  type PendingLine,
  put as cachePut,
  putPending,
  readPendingByTagma,
  readTailBefore,
} from "@kallipai/kallipai-keryx-client";
import type { SendOptions, Transport } from "./transport.ts";
import type {
  HistoryPage,
  MessageSender,
  PullOpts,
} from "./messageSender.svelte.ts";
import {
  HistorySchemaMismatch,
  RelayMessageSender,
} from "./messageSender.svelte.ts";
import { DirectTransport, type TransportState } from "./directTransport.ts";
import { DataPlaneApiError } from "@kallipai/kallipai-lesche-client";
import { KallipaiError } from "@kallipai/kallipai-common";
import {
  chat_send_failed,
  chat_send_timed_out,
} from "../../paraglide/messages.js";

import { PUT_THROTTLE_MS, tagmaKey, unreadStore } from "./unread.svelte.ts";
import { statusCardStore } from "./statusCard.svelte.ts";
import { notify } from "./notify.ts";

/** How long an accepted send waits for its ack/error reply before the
 * watchdog presumes the reply lost. Aligned with the SSE connect timeout
 * and the per-attempt fetch cap (30s each). */
const SEND_WATCHDOG_MS = 30_000;

/** Restart backoff for a dead read path (direct only): first wait, and the cap
 *  the doubling stops at. The stream's own retry ladder covers short gaps; this
 *  covers the case where that ladder gave up. */
const RESTART_MIN_MS = 15_000;
const RESTART_MAX_MS = 60_000;

/** Tunnels already told that the roster names no root agent. An install in
 *  that state can never land a cursor write, so the badge only grows and the
 *  cause is invisible; one line per tunnel says so without repeating it every
 *  throttle window, and a reconnect starts clean (a fixed roster warns again
 *  only if it breaks again). */
const rootlessWarned = new WeakSet<RelayChannel>();

/** Name the rootless failure once per tunnel (see {@link rootlessWarned}). */
function warnRootlessOnce(channel: RelayChannel): void {
  if (rootlessWarned.has(channel)) return;
  rootlessWarned.add(channel);
  console.warn(
    "[read cursor] the tagma roster names no root agent; read cursors are not reported",
  );
}

/** The lazy-window page size: how many lines a hydrate, a catch-up batch,
 *  or a scroll-up page brings in at once. Mirrors the server's
 *  /external/history clamp (50: DEFAULT_LIMIT = MAX_LIMIT, routes/message.rs)
 *  so a full page is one request — if that clamp ever moves, this mirror
 *  drifting only costs an extra partial request, never correctness. */
export const WINDOW_PAGE = 50;

/** Map a cached row back to its transcript line (the hydrate + cache-page
 * paths; the cache stores the UI's own role union verbatim, cast back to
 * the reducer's role union, which the values round-trip as). */
export function cachedLineToLine(c: CachedLine): ConversationLine {
  return {
    historyId: c.historyId,
    role: c.role as ConversationLine["role"],
    text: c.text,
    sender: c.sender,
    createdAt: c.createdAt,
    attachment: c.attachment,
    quote: c.quote,
    seq: c.seq,
  };
}

/** The durable id a content frame carries (0 for markers, acks, and system
 *  lines): the single test every reducer path uses to tell content from
 *  protocol traffic. */
function contentId(reply: TagmaReply): number {
  return reply.kind === "event" || reply.kind === "user_message"
    ? (reply.history_id ?? 0)
    : 0;
}
/** The position a content frame carries (null for markers, acks, and frames
 *  the tagma did not stamp): the content test of {@link contentId}, in the
 *  numbering the resume cursor lives in. */
function contentSeq(reply: TagmaReply): number | null {
  const seq =
    reply.kind === "event" || reply.kind === "user_message" ? reply.seq : null;
  return seq != null && seq > 0 ? seq : null;
}

/** Transport-status surface: the sidebar dot + the chat-page disabled gate. */
export type ConversationStatus =
  | "opening"
  | "open"
  | "reconnecting"
  | "offline"
  | "error";

/** The minimal store surface a Conversation needs (back-ref for the stale
 *  guard). Defined here so conversation.svelte.ts does not import the store
 *  module (avoids a cycle). */
export interface ConversationStoreLike {
  get(id: string): ConversationBase | undefined;
}

export abstract class ConversationBase {
  transcript: ConversationTranscript = $state(EMPTY_TRANSCRIPT);
  status: ConversationStatus = $state("opening");
  /** Transport-level error (the layout banner classifies it). Set when the
   *  drain fails; cleared on a fresh attach. */
  error: unknown = $state(null);
  /** Source of synthetic negative ids for lines without a real history_id.
   *  Plain (the {#each} key is read at line construction, not observed). */
  syntheticSeq = 0;
  /** The most recent pending stamp this session allocated (the monotonic
   *  floor nextPendingSeq decrements below; plain, not observed). */
  lastPendingSeq = 0;

  abstract readonly kind: "local" | "relay" | "offline-view";

  /** The underlying transport, or null once the drain ended for good. `$state`
   *  so consumers re-fire when run() nulls it on a terminal drain end -- not
   *  just on attach/detach. A stream death under a restart does not null it. */
  protected transport = $state<Transport | null>(null);
  /** The write path's carrier: the POST, its settlement and the history pull.
   *  Deliberately not the read-path transport -- a dying stream must not take
   *  the queue with it (see messageSender.svelte.ts). Null only when the
   *  conversation is closed. Reactive, like the sender's own liveness flag:
   *  the write axis changes exactly when a carrier is mounted or its sender
   *  closes, and both fields are `$state` so a reader tracks either flip. */
  protected sender: MessageSender | null = $state(null);
  /** Set once close() has run: the restart wait and run() both stop here, and
   *  the offline view's canWrite reads it (it has no sender to close). */
  protected closed = $state(false);
  /** Rebuild the read-path transport after its drain ended. Null = the drain's
   *  end is terminal (the relay, or a conversation held by a test). */
  protected restartStream: (() => Transport) | null = null;
  private restartDelayMs = RESTART_MIN_MS;
  /** Resolves the in-flight restart wait early (reattach). */
  private restartWake: (() => void) | null = null;

  /** The IndexedDB cache key (the tagma's conversation id). The offline
   *  `"local"` entry and the online derived-id entry for the same tagma share
   *  this so a mode switch rehydrates from the same rows. Falls back to the
   *  store key when the tagma is not enrolled (no durable history). */
  readonly cacheConversationId: string;
  /** The newest durable row id rendered: the window's upper edge, peekRow's
   *  upper bound, and the id the cache pages by. It is not the read
   *  watermark -- a row id says nothing about how far into its own
   *  conversation the row sits. See `lastSeq` for the position the tagma
   *  counts unread against, and `minRowId` for the dedup floor. */
  maxRowId = $state(0);
  /** The newest rendered position (`chat_history.seq`) with no hole below
   *  it: the resume cursor and the read watermark. Positions are dense and
   *  never reused, so one survives a reconnect, and `after_seq = lastSeq`
   *  asks the tagma for exactly what this device has not rendered yet. 0
   *  means no position is known: a cold start opens the newest window rather
   *  than asking from zero, which would pull the whole log. Moved by rows
   *  only -- a stamped frame that continues the run, or a pulled page's
   *  newest row. A frame without a position moves nothing, and a page's
   *  `next` says where the request continues, never where the reader has
   *  rendered to. */
  lastSeq = $state(0);
  /** The oldest rendered position: the scroll-up anchor (`before_seq`).
   *  0 = nothing loaded. */
  minSeq = $state(0);
  /** True while a repair pass runs (single-flight guard). */
  private resuming = false;
  /** Set when a repair is asked for while one runs: the running pass takes
   *  one more sweep instead of stacking pulls. */
  private resumeAgain = false;
  /** The probe in flight: a catch-up joining it shares its answer instead of
   *  asking the tagma the same question again. */
  private probe: Promise<void> | null = null;
  /** Rendered optimistic user lines awaiting their POST. Each entry's line is
   *  already in `transcript` (status "sending"); the single-in-flight send pump
   *  drains this one ack at a time. */
  pending = $state<
    {
      localId: number;
      text: string;
      attachment?: FileAttachment;
      quote?: number;
    }[]
  >([]);

  /** Discarded local ids. Rule: any read that redraws lines from the store
   *  consults this first -- a row can be in a snapshot taken before the delete
   *  landed, and a discard must not be undone by a reconnect. */
  readonly discarded = new Set<number>();
  /** Watchdog for the in-flight POST: if neither the ack nor an error
   *  reply consumes `pendingInFlight` within the window, the pump would
   *  stall forever (a lost reply rides no transport failure). The timer
   *  fails the line (retryable) and releases the slot. */
  private pumpWatchdog: ReturnType<typeof setTimeout> | null = null;
  /** The ONE in-flight POST (its `user_message` frame has not landed): its
   *  synthetic id + sent text, or null when the pump is idle. The text lets the
   *  promotion branch correlate the echo to this exact send, so a history-replay
   *  `user_message` arriving mid-flight is not mistaken for the ack. */
  pendingInFlight = $state<{
    localId: number;
    text: string;
    reqId?: number;
  } | null>(null);
  /** The latest aggregate status snapshot (root state, subagent counts, token
   *  budget) for THIS conversation's tagma, surfaced to the chat header. The
   *  single uniform source for the header regardless of transport: drained from
   *  `Transport.status()` (direct SSE) for offline, set via
   *  {@link setStatusSnapshot} from the realtime status sink for online (the
   *  relay transport carries no status). `undefined` until the first snapshot. */
  statusSnapshot = $state<
    import("../tagmata.svelte.ts").TagmaStatusSummary | undefined
  >(undefined);

  /** The oldest durable id in the loaded window (the scroll-up cursor).
   *  `0` = nothing loaded. Together with `maxRowId` it brackets what
   *  the transcript currently holds; rows outside the bracket are fetched
   *  on demand (catch-up above, scroll-paging below), never up front.
   *  Second duty, dedup: a content frame below this floor was rendered and
   *  then let go, so it is a replay; anything at or above the floor is still
   *  eligible to land. */
  minRowId = $state(0);
  /** True while a scroll-up page is in flight (single-flight guard + the
   *  top sentinel's busy state). */
  loadingOlder = $state(false);
  /** False once an older page came back empty — the sentinel stops
   *  arming. Stays true while unknown (conservative: an extra empty
   *  pull is cheap, a missed page is not). */
  hasMoreOlder = $state(true);

  /** Set when the history could not be brought up -- a pull that failed, a
   *  cache that could not open, or a page written under a numbering this
   *  build does not read: that last one is `"protocol"`, which freezes the
   *  window (see {@link historyFrozen}). The page shows it with a retry, or
   *  with a reload when no retry can help, instead of an empty transcript
   *  that reads as an empty conversation. Cleared by the next page that
   *  lands. */
  historyError = $state<null | "unavailable" | "protocol">(null);

  /** True once the tagma answered under a numbering this build does not
   *  read: the window keeps rendering what arrives and stops placing it -- no
   *  position stamp, no cursor move, no cache row, no repair -- because a
   *  position from another numbering orders rows against a different log.
   *  Only a reload ends it: the disagreement is between the running bundle and
   *  the tagma, not between one request and its answer. */
  get historyFrozen(): boolean {
    return this.historyError === "protocol";
  }

  /** Record a failed history pull: an answer written under another numbering
   *  freezes the window, anything else is the retryable case. A freeze is
   *  never downgraded -- the mismatch stands until a reload, whatever fails
   *  next. */
  protected notePullFailure(e: unknown): void {
    if (this.historyFrozen) return;
    this.historyError =
      e instanceof HistorySchemaMismatch ? "protocol" : "unavailable";
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
    this.transport = transport;
    this.sender = sender;
    this.cacheConversationId = cacheConversationId;
  }

  get connected(): boolean {
    // True while the conversation holds its destination: writable, a stream
    // holder, and a stream that is not reconnecting. Both layers report the
    // same fact on `status`: the transport's own SSE retry says
    // "reconnecting" through onTransportState, and a drain end under a
    // restart says it in run(). A stream death is not a disconnect, so the
    // status -- not the transport's presence -- is what flips this false.
    // The relay panel's channel window is the same predicate: its drain end is
    // terminal, so it nulls the transport rather than going "reconnecting".
    // Built on canWrite so the two cannot drift: this is the lamp, that is the
    // write axis.
    return (
      this.canWrite && this.transport !== null && this.status !== "reconnecting"
    );
  }
  /** The write axis: whether a line typed now has a path to land. Two inputs:
   *  the conversation is open, and this leaf's carrier exists (`carrierLive`).
   *  Both are runes (the fields behind them are `$state`), so a page deriving
   *  from this re-evaluates on close and on a carrier changing under it. A
   *  stream rebuilding leaves both true; `connected` adds stream health. */
  get canWrite(): boolean {
    return !this.closed && this.carrierLive;
  }

  /** The leaf's share of the write axis: does the path a line travels exist
   *  right now. The direct leaf's carrier outlives its stream, so this is a
   *  mounted sender that has not closed; the relay leaf's path is its own
   *  channel; the offline view writes into the pending store. `available` says
   *  'this sender was not closed', never 'a send can go through now'. */
  protected get carrierLive(): boolean {
    return this.sender !== null && this.sender.available;
  }

  /** Tear down the underlying transport. The run() drain then ends; its stale
   *  guard stops it from mutating the (likely-removed) conversation. */
  close(): void {
    this.transport?.close();
    this.sender?.close();
    this.closed = true;
    this.restartWake?.();
  }
  /** Send a user message online: renders the optimistic line, queues it,
   *  and hands off to the shared single-in-flight send pump (the in-flight
   *  POST's `user_message` frame promotes the line via `applyReplyCore`). */
  send(text: string, options?: SendOptions): void {
    if (!this.canWrite) {
      this.error = new Error(
        this.closed ? "conversation closed" : "not connected",
      );
      return;
    }
    const trimmed = text.trim();
    if (trimmed === "" && options?.attachment === undefined) return;
    const localId = this.renderPendingLine(trimmed, options);
    this.pending = [
      ...this.pending,
      {
        localId,
        text: trimmed,
        attachment: options?.attachment,
        quote: options?.quote,
      },
    ];
    void this.pumpPending();
  }

  /** Render + persist one locally authored line: the shared core of the
   *  online send and the offline shell's send. The caller trims and
   *  empty-gates the text; this takes the next localSeq, appends the
   *  optimistic bubble, and writes the durable pending row (removed when
   *  the ack lands). Returns the localSeq, which doubles as the rendered
   *  line's historyId. */
  protected renderPendingLine(trimmed: string, options?: SendOptions): number {
    const attachment = options?.attachment;
    const quote = options?.quote;
    const now = new Date();
    const localId = nextPendingSeq(now.getTime(), this.lastPendingSeq);
    this.lastPendingSeq = localId;
    this.transcript = withUserLine(
      this.transcript,
      trimmed,
      localId,
      this.localSender,
      attachment,
      now,
      quote,
    );
    void putPending({
      tagmaId: this.pendingKey,
      localSeq: localId,
      text: trimmed,
      ...(attachment !== undefined ? { attachment } : {}),
      ...(quote !== undefined ? { quote } : {}),
      createdAt: now.toISOString(),
      status: "queued",
    });
    return localId;
  }

  /** The pending-store partition this conversation's unsent lines live
   *  under (the tagma id; the offline shell's local conversation uses the
   *  literal "local", mirroring its cache-key fallback). */
  abstract get pendingKey(): string;

  /** Re-send one failed line (the bubble's retry button). Idempotent: a
   *  line that is not failed, already in flight, or already queued is a
   *  no-op. */
  retrySend(localSeq: number): void {
    const line = this.transcript.lines.find((l) => l.historyId === localSeq);
    if (!line || line.status !== "failed") return;
    if (this.pendingInFlight?.localId === localSeq) return;
    if (this.pending.some((p) => p.localId === localSeq)) return;
    this.transcript = retryLine(this.transcript, localSeq);
    this.pending = [
      ...this.pending,
      {
        localId: localSeq,
        text: line.text,
        attachment: line.attachment,
        quote: line.quote,
      },
    ];
    void this.pumpPending();
  }

  /** Discard one failed line for good (the bubble's delete action). The text,
   *  the attachment and the quote go with the line, and the durable pending row
   *  goes too, so a reload cannot draw it again: an online failure's row is
   *  `failed`, an offline line's row is `queued`, and both are removed here (the
   *  offline one is a send that will never happen). Only a failed line is
   *  discardable -- a sending one is under way, and a sent one is history.
   *
   *  Returns whether the line left. The write is synchronous, so the caller
   *  (the bubble's confirm) can tell a real discard from a refused one before
   *  the row re-renders, and place focus accordingly. */
  discard(localSeq: number): boolean {
    const line = this.transcript.lines.find((l) => l.historyId === localSeq);
    if (!line || line.status !== "failed") return false;
    if (this.pendingInFlight?.localId === localSeq) return false;
    this.transcript = dropFailedLine(this.transcript, localSeq);
    this.pending = this.pending.filter((p) => p.localId !== localSeq);
    this.discarded.add(localSeq);
    void deletePending(this.pendingKey, localSeq);
    return true;
  }

  /** Drain every persisted pending row after (re)connect. The partition:
   *  queued rows (never accepted) are auto-sent -- at-least-once by design,
   *  a row the server accepted but whose ack never landed re-sends a
   *  duplicate -- while FAILED rows are only re-rendered (the failure copy
   *  rides the line), never auto-resent: a failed send must wait for an
   *  explicit user retry. Lines a reload wiped from memory are re-rendered
   *  from the durable copy first, so the bubble is never lost. */
  async retryAllPending(): Promise<void> {
    let rows: PendingLine[] = [];
    try {
      rows = await readPendingByTagma(this.pendingKey);
    } catch {
      return; // IndexedDB unavailable (private mode): nothing to flush
    }
    for (const row of rows) {
      if (this.discarded.has(row.localSeq)) {
        // Discarded after this read began: the line is gone from the transcript
        // and must not come back. Re-issuing the delete heals a miss.
        void deletePending(this.pendingKey, row.localSeq);
        continue;
      }
      const failed = isFailedPending(row);
      // Render (or re-render) in stored order, whatever the branch: the
      // bubble sequence must follow the rows' original timing.
      if (!this.transcript.lines.some((l) => l.historyId === row.localSeq)) {
        let t = withUserLine(
          this.transcript,
          row.text,
          row.localSeq,
          this.localSender,
          row.attachment,
          new Date(row.createdAt),
          row.quote,
        );
        if (failed) t = sendFailed(t, row.localSeq, row.lastError);
        this.transcript = t;
      }
      if (failed) continue; // wait for an explicit retry
      // Queued: enqueue directly (retrySend's failed-state guard does not
      // apply -- this row never failed; it is waiting for its first send).
      if (this.pendingInFlight?.localId === row.localSeq) continue;
      if (this.pending.some((p) => p.localId === row.localSeq)) continue;
      this.pending = [
        ...this.pending,
        {
          localId: row.localSeq,
          text: row.text,
          attachment: row.attachment,
          quote: row.quote,
        },
      ];
    }
    if (this.pending.length > 0) void this.pumpPending();
  }

  /** Hook for leaves to react to a reply AFTER the shared core reduce (the relay
   *  flips `live` on `history_batch_end` and fires background notifications). */
  protected onReply(_reply: TagmaReply): void {}

  /** Apply one authored reply through the shared core: dedup by `history_id`,
   *  promote an optimistic line on a stamped `user_message`, reduce, cache, and
   *  advance the cursor. `sender` is the wire participant who authored the
   *  reply's content. Guarded by `isLive` so a stale drain cannot touch a
   *  fresher entry. */
  protected applyReplyCore(
    reply: TagmaReply,
    sender: Participant | undefined,
  ): void {
    // A `user_message` echo closes the in-flight optimistic line -- but only
    // when it is THIS send's echo. Correlate by text: a history-replay
    // `user_message` (e.g. a relay catch-up row) arriving while a fresh local
    // send is in-flight must NOT be consumed as the ack (the echo carries no
    // req_id to correlate on). A stamped echo (history_id > 0) promotes the
    // line to the durable id; an unstamped echo (history_id === 0, a
    // direct-path echo or a DB-write failure) keeps the synthetic id and just
    // flips "sending" -> "sent". Either way the echo is consumed (never
    // appended as a duplicate) and the send pump advances. A non-matching echo
    // falls through to the normal dedup/append path below. Residual edge: a
    // catch-up row whose text identically matches the in-flight send still
    // collides -- far rarer than the prior uncorrelated misfire.
    // An op error answering the in-flight send (correlated by req_id):
    // the message never landed -- e.g. the peer tagma is parked -- so close
    // the optimistic line, surface the server copy as the single red
    // error, and release the pump slot. Unmatched op errors keep the wire
    // path below: a durable system line + red, the server-error record.
    if (isInFlightError(reply, this.pendingInFlight)) {
      const localId = this.pendingInFlight!.localId;
      this.transcript = sendFailed(
        this.transcript,
        localId,
        reply.message,
        reply.code,
      );
      void markPendingFailed(this.pendingKey, localId, reply.message);
      this.clearWatchdog();
      this.pendingInFlight = null;
      // The verbatim operator text stays in the console for diagnosis; the
      // UI shows the localized short form keyed by the code (when present).
      console.warn(`[send] rejected (${reply.status}):`, reply.message);
      void this.pumpPending();
      this.onReply(reply);
      return;
    }
    // A frame this window already renders is a replay, whatever else is in
    // flight: its row is on screen, so it never claims a queued line (two
    // identical texts would otherwise hand the echo to the wrong one).
    const realId = contentId(reply);
    const replayed =
      realId > 0 && this.transcript.lines.some((l) => l.historyId === realId);
    if (replayed) {
      // The row is on screen already: the frame only hands it its position.
      this.adoptPosition(reply, realId, sender);
      this.onReply(reply);
      return;
    }
    if (
      reply.kind === "user_message" &&
      this.pendingInFlight !== null &&
      reply.text.trim() === this.pendingInFlight.text.trim()
    ) {
      const localId = this.pendingInFlight.localId;
      const ackId = reply.history_id;
      if (ackId > 0) {
        // The wire sender is authoritative for the confirmed line: overwrite the
        // optimistic line's (stale, client-side) sender so a handle that changed
        // mid-session does not freeze on the old value, matching every other path
        // where the wire sender drives the rendered/cached sender.
        const wireSender = sender ? toSender(sender) : undefined;
        this.transcript = replaceLineId(
          this.transcript,
          localId,
          ackId,
          reply.created_at ?? undefined,
          wireSender,
        );
        this.adoptPosition(reply, ackId, sender);
        if (ackId > this.maxRowId) this.maxRowId = ackId;
        // The send landed: the durable pending copy is no longer needed.
        void deletePending(this.pendingKey, localId);
      } else {
        // Unstamped echo: keep the synthetic line, flip it to "sent". The echo
        // carries no new content, so drop it (never append) -- otherwise it
        // would render a second bubble and the send pump would never advance.
        this.transcript = markLineSent(this.transcript, localId);
        // Landed (direct-path ack): drop the durable pending copy too.
        void deletePending(this.pendingKey, localId);
      }
      this.pendingInFlight = null;
      this.clearWatchdog();
      void this.pumpPending();
      this.onReply(reply);
      return;
    }
    // Dedup: a frame below the window's floor was rendered and then let go.
    // Anything between that floor and the newest row is a row this
    // conversation never landed (a settled line can leave such a hole), and
    // it lands now.
    if (this.minRowId > 0 && realId > 0 && realId < this.minRowId) {
      this.onReply(reply);
      return;
    }
    const seq = contentSeq(reply);
    // A position at or below the cursor is already covered: the row is on
    // screen, or was before paging let it go, so the frame is a replay and
    // nothing about it may move the cursor again.
    // A frozen window has no cursor to compare against: its positions come
    // from another numbering, so frames render and only row ids dedup.
    if (!this.historyFrozen && seq !== null && seq <= this.lastSeq) {
      this.onReply(reply);
      return;
    }
    const lineId = realId > 0 ? realId : (this.syntheticSeq -= 1);
    this.transcript = applyTagmaReply(this.transcript, reply, sender, lineId);
    // The position a frame carries lands with the line it renders: a row that
    // arrived on the stream is as placeable as one a page delivered, so a
    // later page merging this id has nothing left to fill in.
    if (!this.historyFrozen && seq !== null) {
      this.transcript = stampPosition(this.transcript, lineId, seq);
    }
    const cl = cacheLineOf(reply, sender);
    // A frozen window keeps rows off the cache: a cached row carries a
    // position, and this build cannot tell which numbering minted it.
    if (cl) {
      if (!this.historyFrozen) {
        void cachePut({
          conversationId: this.cacheConversationId,
          historyId: cl.historyId,
          role: cl.role,
          text: cl.text,
          sender: cl.sender,
          createdAt: cl.createdAt,
          attachment: cl.attachment,
          quote: cl.quote,
          seq: cl.seq,
        });
      }
      if (cl.historyId > this.maxRowId) this.maxRowId = cl.historyId;
    }
    // Move the resume cursor over the position this frame carries. A frame
    // that jumps above the cursor still renders (its content is real), but
    // the cursor stays below the hole so one repair pulls the missing rows
    // back; an unstamped frame moves nothing.
    if (!this.historyFrozen && seq !== null) {
      if (seq === this.lastSeq + 1) this.advancePositions([seq]);
      else if (seq > this.lastSeq + 1) void this.resume();
    }
    this.onReply(reply);
  }

  /** Single-in-flight send pump. POSTs the next queued optimistic line (if
   *  any, and if no POST is already outstanding), leaving its localId + text
   *  in `pendingInFlight` so the stamped `user_message` frame can correlate.
   *  On a POST failure the line is kept and marked failed via `sendFailed`
   *  (retryable), with the failure copy inline. */
  protected async pumpPending(): Promise<void> {
    if (this.pendingInFlight !== null) return;
    const sender = this.sender;
    // A missing or closed sender leaves the line queued: the pump is the queue's
    // writer, not its owner, so nothing is dropped here.
    if (sender === null || !sender.available) return;
    const next = this.pending.shift();
    if (next === undefined) return;
    this.pendingInFlight = { localId: next.localId, text: next.text };
    try {
      const result = await sender.send(next.text, {
        attachment: next.attachment,
        quote: next.quote,
      });
      // A response-borne id settles the line here, with no help from the read
      // path. Only a response without one falls back to the wire ack (relay
      // frame) or the echo plus watchdog (direct degraded write).
      if (
        result.historyId !== null &&
        result.historyId > 0 &&
        this.pendingInFlight?.localId === next.localId
      ) {
        this.settleAccepted(next.localId, result.historyId);
        this.pendingInFlight = null;
        void this.pumpPending();
      }
      if (result.historyId === null || result.historyId <= 0) {
        this.armWatchdog(next.localId);
      }
      // The channel stamps this send's req_id on accept; the reply-side
      // error correlation (applyReplyCore) closes the line on it.
      if (this.pendingInFlight?.localId === next.localId) {
        this.pendingInFlight = {
          ...this.pendingInFlight,
          reqId: result.reqId,
        };
      }
    } catch (e) {
      // Typed failures (direct KallipaiError / relay DataPlaneApiError) keep
      // their server copy; transport failures are qualitative.
      let failureCopy: string;
      if (e instanceof KallipaiError || e instanceof DataPlaneApiError) {
        failureCopy = e.message;
      } else {
        console.error("[chat] send failed:", e);
        failureCopy = chat_send_failed();
      }
      // Local send failure: red status error only -- no system history
      // line. A genuine server kind:"error" reply (the wire path)
      // still enters history via the reducer; that is by-design and
      // stays. Feeding this failure through the same reducer is what
      // double-rendered it (system line + red banner).
      this.transcript = sendFailed(this.transcript, next.localId, failureCopy);
      void markPendingFailed(this.pendingKey, next.localId, failureCopy);
      this.pendingInFlight = null;
      this.clearWatchdog();
      void this.pumpPending();
    }
  }

  /** Promote a posted line to its confirmed row. The response's id is the
   *  authority: the line renumbers, caches and clears its pending copy here,
   *  with no read path involved. The cursor does not move: the response and
   *  the stream are separate channels, and a lower row may still be on the way.
   *  The echo that follows drops as a replay by its id. The row keeps the
   *  client's created_at until a pull supplies the server's. */
  private settleAccepted(localId: number, ackId: number): void {
    if (ackId <= 0) return;
    // The line is this client's own, so its sender is already the local one.
    const wireSender = this.localSender;
    this.transcript = replaceLineId(
      this.transcript,
      localId,
      ackId,
      undefined,
      wireSender,
    );
    const confirmed = this.transcript.lines.find((l) => l.historyId === ackId);
    if (confirmed && !this.historyFrozen) {
      // A frozen window writes no cache rows: the id this row just took
      // belongs to a numbering this build does not read, and a cached row
      // is a position the next hydrate would trust.
      // Plain `wireSender`, never the $state proxy (structured clone rejects it).
      void cachePut({
        conversationId: this.cacheConversationId,
        historyId: ackId,
        role: "user",
        text: confirmed.text,
        sender: wireSender,
        createdAt: confirmed.createdAt,
        attachment: confirmed.attachment,
        quote: confirmed.quote,
      });
    }
    void deletePending(this.pendingKey, localId);
  }

  /** Arm the reply watchdog for one in-flight send. 30s, matching the SSE
   *  connect timeout and the per-attempt fetch cap: past that, a reply is
   *  presumed lost. Fails the line (retryable, no auto-resend) and frees
   *  the pump slot. */
  private armWatchdog(localId: number): void {
    this.clearWatchdog();
    this.pumpWatchdog = setTimeout(() => {
      this.pumpWatchdog = null;
      if (this.pendingInFlight?.localId !== localId) return;
      const copy = chat_send_timed_out();
      this.transcript = sendFailed(this.transcript, localId, copy);
      void markPendingFailed(this.pendingKey, localId, copy);
      this.pendingInFlight = null;
      console.warn("[chat] send reply lost; failed the line after 30s");
      void this.pumpPending();
    }, SEND_WATCHDOG_MS);
  }

  protected clearWatchdog(): void {
    if (this.pumpWatchdog !== null) {
      clearTimeout(this.pumpWatchdog);
      this.pumpWatchdog = null;
    }
  }
  /** Fold one page of already-renderable lines into the window in a single
   *  transcript rebuild (the batch — not the row — is the update unit, so a
   *  50-row page costs one O(window) pass, not 50). Pure w.r.t. the
   *  transcript; the window edges and the scroll-up position recompute
   *  from the merged lines.
   *  Returns how many rows were newly added (0 = the page was fully
   *  redundant — the idempotency signal loadOlder uses to stop paging). */
  protected mergeWindowLines(rows: ConversationLine[]): number {
    const { transcript, added } = mergeHistoryLines(this.transcript, rows);
    // A page can name the position of a row that is already on screen
    // without adding a line: that fill is still a change, so it is applied
    // before the idempotency answer the caller pages on.
    if (added === 0 && transcript === this.transcript) return 0;
    this.transcript = transcript;
    let min = Infinity;
    let max = 0;
    let minSeq = Infinity;
    for (const l of transcript.lines) {
      if (l.historyId > 0) {
        if (l.historyId < min) min = l.historyId;
        if (l.historyId > max) max = l.historyId;
      }
      if (l.seq != null && l.seq > 0 && l.seq < minSeq) minSeq = l.seq;
    }
    if (min !== Infinity) this.minRowId = min;
    if (max > this.maxRowId) this.maxRowId = max;
    if (minSeq !== Infinity) this.minSeq = minSeq;
    return added;
  }

  /** Ingest one pulled history batch: persist every row to the cache first
   *  (the put is historyId-keyed, so re-pulls are idempotent), then fold
   *  the rows into the window. Resume paging and scroll paging both land
   *  here — the pulled path must cache what it renders, because unlike the
   *  live drain it bypasses applyReplyCore's cachePut. */
  protected applyPulledRows(rows: HistoryEntry[]): number {
    const lines = rows
      .map(historyEntryLine)
      .filter((l): l is ConversationLine => l !== null);
    // A page already in flight when the freeze landed still renders below:
    // it hands over neither a cache row nor a position.
    if (!this.historyFrozen) {
      for (const l of lines) {
        void cachePut({
          conversationId: this.cacheConversationId,
          historyId: l.historyId,
          role: l.role,
          text: l.text,
          sender: l.sender,
          createdAt: l.createdAt,
          attachment: l.attachment,
          quote: l.quote,
          seq: l.seq,
        });
      }
      this.advancePositions(lines.map((l) => l.seq ?? 0));
    }
    return this.mergeWindowLines(lines);
  }
  /** Recompute the window floor (`minRowId`) and the scroll-up position
   *  (`minSeq`) from the rendered lines. The drain-side
   *  batch path appends rows through applyReplyCore (which advances only
   *  `maxRowId`); this closes the bracket once a batch has landed so the
   *  scroll sentinel can arm. The floor only moves down and the scroll-up
   *  position only moves up, so a window that lost its oldest rendered row
   *  keeps a floor it already proved. Idempotent, O(window). */
  protected recomputeMinRendered(): void {
    let min = Infinity;
    let minSeq = Infinity;
    for (const l of this.transcript.lines) {
      if (l.historyId > 0 && l.historyId < min) min = l.historyId;
      if (l.seq != null && l.seq > 0 && l.seq < minSeq) minSeq = l.seq;
    }
    if (min !== Infinity) this.minRowId = min;
    if (minSeq !== Infinity) this.minSeq = minSeq;
  }
  /** Adopt the rendered positions of a window that is already on screen (a
   *  cache hydrate): its newest position becomes the resume cursor, its oldest
   *  the scroll-up anchor, and the row-id edges follow the same lines. A
   *  hydrate is the one place a whole window arrives at once -- every other
   *  landing comes through applyReplyCore or applyPulledRows. A frozen
   *  window has no positions to adopt, so it places nothing. */
  seedRenderedPositions(lines: readonly ConversationLine[]): void {
    if (this.historyFrozen) return;
    let minId = Infinity;
    let maxId = 0;
    for (const l of lines) {
      if (l.historyId > 0) {
        if (l.historyId < minId) minId = l.historyId;
        if (l.historyId > maxId) maxId = l.historyId;
      }
    }
    if (minId !== Infinity) this.minRowId = minId;
    if (maxId > this.maxRowId) this.maxRowId = maxId;
    this.recomputeMinRendered();
    this.advancePositions(lines.map((l) => l.seq ?? 0));
  }

  /** Move the resume cursor over positions this client just rendered. Only a
   *  position that continues the run at `lastSeq + 1` advances it, so a row
   *  landing above a hole never carries the cursor over that hole. A cursor
   *  with nothing behind it (0) takes the lowest rendered position and runs on
   *  from there: that window is the device's own start. */
  protected advancePositions(seqs: readonly number[]): void {
    const above = seqs.filter((s) => s > this.lastSeq).sort((a, b) => a - b);
    let cursor = this.lastSeq;
    for (const s of above) {
      if (cursor === 0 || s === cursor + 1) cursor = s;
      else break;
    }
    if (cursor === this.lastSeq) return;
    this.lastSeq = cursor;
    this.reportPosition(cursor);
  }

  /** Adopt the position a frame reports for a row the transcript already
   *  renders: an ack-promoted line, or a replay of a settled one. The line
   *  takes the position, its cache copy follows, and a position that continues
   *  the run moves the cursor. A jump does not resume: the row is on screen,
   *  so no hole sits under it. */
  private adoptPosition(
    reply: TagmaReply,
    historyId: number,
    sender: Participant | undefined,
  ): void {
    const seq = contentSeq(reply);
    if (historyId <= 0) return;
    // Frozen: the row renders, but its position comes from a numbering
    // this build cannot place (see `historyFrozen`).
    if (this.historyFrozen) return;
    if (seq !== null) {
      this.transcript = stampPosition(this.transcript, historyId, seq);
      if (seq === this.lastSeq + 1) this.advancePositions([seq]);
    }
    const cached = cacheLineOf(reply, sender);
    if (cached === null) return;
    void cachePut({
      conversationId: this.cacheConversationId,
      historyId: cached.historyId,
      role: cached.role,
      text: cached.text,
      sender: cached.sender,
      createdAt: cached.createdAt,
      attachment: cached.attachment,
      quote: cached.quote,
      seq: cached.seq,
    });
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
    const sender = this.sender;
    const pull = sender?.pullHistory;
    if (pull === undefined) return;
    // A frozen window pulls nothing: every page would carry positions this
    // build cannot place, and a reload is the way out.
    if (this.historyFrozen) return;
    if (this.resuming) {
      this.resumeAgain = true;
      return;
    }
    this.resuming = true;
    try {
      do {
        this.resumeAgain = false;
        await this.repairOnce(pull.bind(sender), k);
      } while (this.resumeAgain && !this.closed);
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
    if (this.historyFrozen) return;
    let cursor = this.lastSeq;
    let anchor = this.lastSeq;
    try {
      while (!this.closed) {
        cursor = this.lastSeq;
        anchor = Math.max(anchor, cursor);
        if (cursor <= 0) {
          await this.openNewest(pull, k);
          return;
        }
        const page = await pull({ afterSeq: anchor, limit: k });
        this.historyError = null;
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
          this.lastSeq = first - 1;
        }
        if (page.rows.length > 0) this.applyPulledRows(page.rows);
        // `more` answers what lies above the page and `next` where the same
        // request continues. The anchor follows `next`; the cursor follows
        // rendered rows. A page whose tail did not decode leaves the cursor
        // behind its edge: that block is stepped over once, with the block
        // named, so the sweep does not ask for it again on every frame. The
        // step happens even when the block runs to the newest row, where no
        // page above it exists to carry the cursor forward.
        const edge = page.next;
        if (edge === null || edge <= anchor) return;
        if (this.lastSeq < edge && page.undecodable > 0) {
          console.warn(
            "[chat] stepping from " +
              this.lastSeq +
              " to " +
              edge +
              "; " +
              page.undecodable +
              " row(s) in this page did not decode, so part of that span" +
              " is not on screen",
          );
          this.lastSeq = edge;
        }
        if (!page.more) return;
        anchor = edge;
      }
    } catch (e) {
      // A failed pull is not a filled hole: the cursor stays where it was, so
      // the next frame or the next mount asks again.
      console.warn("[chat] resume failed above " + cursor + ":", e);
      this.notePullFailure(e);
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
    const unsent = this.transcript.lines.filter(
      (line) =>
        this.pending.some((entry) => entry.localId === line.historyId) ||
        line.historyId === this.pendingInFlight?.localId,
    );
    this.transcript = { ...EMPTY_TRANSCRIPT, lines: unsent };
    this.lastSeq = 0;
    this.minSeq = 0;
    this.minRowId = 0;
    this.maxRowId = 0;
    this.hasMoreOlder = true;
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
    if (this.historyFrozen) return;
    try {
      const page = await pull({ limit: k });
      this.historyError = null;
      if (page.rows.length > 0) this.applyPulledRows(page.rows);
      // A window short of k raw rows IS all the history there is; one that
      // arrived complete proves nothing about what lies below it.
      if (page.rows.length + page.undecodable < k) this.hasMoreOlder = false;
    } catch (e) {
      // Dead carrier: the drain surfaces the status, the window stays as it
      // is, and live frames keep arriving; say so anyway, because a cold
      // start that quietly shows nothing looks like an empty log.
      console.warn("[chat] could not open the newest window:", e);
      this.notePullFailure(e);
    }
  }

  /** Fetch one page older than the window head. Template method: the
   * shared guards, the single-flight flag, and the quiet error handling
   * live here; the leaf supplies the page source via loadOlderPage.
   *
   *  minRowId === 0 means the window has not formed yet (hydrate or
   *  catch-up still in flight, or the server truly has nothing): paging
   *  older-than-nothing is ill-defined — and racing the initial fill here
   *  once issued a recent-N pull whose always-false `more` permanently
   *  disarmed the sentinel. Wait for a window head to exist. */
  async loadOlder(k = WINDOW_PAGE): Promise<void> {
    // A frozen window pages nothing: rows would land without positions and
    // without a cache copy, so it stops asking until a reload.
    if (this.historyFrozen) return;
    if (this.loadingOlder || !this.hasMoreOlder) return;
    if (this.minRowId <= 0) return;
    this.loadingOlder = true;
    try {
      await this.loadOlderPage(k);
    } catch {
      // Offline / dead transport: retry on the next scroll-to-top.
    } finally {
      this.loadingOlder = false;
    }
  }

  /** Look up one history row by id WITHOUT touching the loaded window.
   *  A quote block asks whether its target still exists, and a page of the
   *  target's neighbourhood must never be merged: the pager only moves
   *  older from the window floor, so a mid-history segment would leave a
   *  hole that nothing can fill.
   *
   *  The decision is made here, once, for every leaf: an id at or below the
   *  rendered head is read through the leaf's own page source, and the
   *  answer is the row itself, `"deleted"` (the page held only rows below
   *  the id, or none at all: a contiguous history without it proves it is
   *  gone -- and only a COMPLETE page proves contiguity: a window the tagma
   *  could not decode whole is a hole, so its miss proves nothing) or
   *  `"error"` (an id that is not a positive integer, an id above the head,
   *  a leaf that cannot give the page, a page with undecodable rows or one
   *  that arrived short of its own count, or a page row that maps to no
   *  line -- an unreadable row could be the quoted one, and nothing could
   *  be drawn for it). The split is never a leaf's call. */
  async peekRow(id: number): Promise<ConversationLine | "deleted" | "error"> {
    if (!Number.isInteger(id) || id <= 0) return "error";
    // A probe is only meaningful for a rendered row: an id above the head
    // is not something a quote can point at, and its page would land above
    // the window as a live row (the reroute only catches ids at or below
    // the head), which is the hole the probe is meant never to open.
    if (id > this.maxRowId) return "error";
    const rows = await this.probePage(id);
    if (rows === null) return "error";
    for (const row of rows) {
      const line = historyEntryLine(row);
      if (line === null) return "error";
      if (line.historyId === id) return line;
    }
    return "deleted";
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

  /** True iff this conversation is still the store's live entry for its id. */
  protected isLive(): boolean {
    return this.store.get(this.conversationId) === this;
  }

  /** Set the status snapshot from outside the transport drain (the online path:
   *  the realtime feed routes `tagma_status` here via the store). Stale-guarded
   *  so a snapshot for a since-replaced entry cannot resurrect it. */
  setStatusSnapshot(
    snapshot: import("../tagmata.svelte.ts").TagmaStatusSummary | undefined,
  ): void {
    if (!this.isLive()) return;
    this.statusSnapshot = snapshot;
  }

  /** Drain all three transport streams concurrently for one transport lifetime.
   *  Sets status to error/offline when the streams end; the caller decides whether
   *  that end is terminal (relay) or a rebuild (direct). */
  private async drainOnce(t: Transport): Promise<void> {
    let failure: unknown = null;
    const drainReplies = async () => {
      try {
        for await (const { sender, reply } of t.replies()) {
          if (!this.isLive()) return;
          this.applyReplyCore(reply, sender);
        }
      } catch (e) {
        if (this.isLive()) failure = e;
      }
    };
    const drainSignals = async () => {
      try {
        for await (const signal of t.signals()) {
          if (!this.isLive()) return;
          this.transcript = applySignal(
            this.transcript,
            signal,
            (this.syntheticSeq -= 1),
          );
        }
      } catch {
        // A signal-drain failure coincides with the reply drain's (same
        // transport); the reply drain records it. Ignore here.
      }
    };
    const drainStatus = async () => {
      try {
        for await (const snapshot of t.status()) {
          if (!this.isLive()) return;
          this.statusSnapshot = snapshot;
          // The direct drain is the drawer summary's only source on the
          // offline shell (the relay path mirrors via the shell's status
          // sink), so each snapshot lands in the store here.
          statusCardStore.setSummary(snapshot);
        }
      } catch {
        // Same as signals: a status-drain failure coincides with the reply
        // drain's; the reply drain records it.
      }
    };
    await Promise.allSettled([drainReplies(), drainSignals(), drainStatus()]);
    if (!this.isLive()) return;
    if (failure !== null) {
      this.status = "error";
      this.error = failure;
    } else if (this.status === "opening" || this.status === "open") {
      this.status = "offline";
    }
  }

  /**
   * Drain the read path, rebuilding it under the same sender when it dies.
   *
   *  The direct stream is replaceable (a fresh transport over the same client and
   *  agent), so a dead stream is stream maintenance, not the end of the session:
   *  the wait is bounded by a backoff the conversation owns, and reattach() cuts
   *  it short. The relay has no replacement, so its drain end stays terminal and
   *  the conversation drops to offline/error exactly as before.
   */
  async run(): Promise<void> {
    for (;;) {
      const t = this.transport;
      if (t === null) return;
      await this.drainOnce(t);
      if (!this.isLive() || this.closed) return;
      if (this.restartStream === null) {
        this.onDrainDead();
        this.transport = null;
        return;
      }
      this.status = "reconnecting";
      const rebuilt = await this.waitForRestart();
      if (rebuilt === null) return;
      this.transport = rebuilt;
      this.onStreamAttached();
    }
  }

  /** Wait out the restart backoff, cut short by {@link reattach}. Returns the
   *  rebuilt transport, or null when the conversation stopped being live. */
  private async waitForRestart(): Promise<Transport | null> {
    const delay = this.restartDelayMs;
    this.restartDelayMs = Math.min(delay * 2, RESTART_MAX_MS);
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        this.restartWake = null;
        resolve();
      }, delay);
      this.restartWake = () => {
        clearTimeout(timer);
        this.restartWake = null;
        resolve();
      };
    });
    if (!this.isLive() || this.closed) return null;
    return this.restartStream?.() ?? null;
  }

  /** Rebuild the read path now instead of on the timer: a foreground return or
   *  the banner's action. A no-op while the wait is not running.
   *
   *  NOT wired in production: DirectTransport owns the visibilitychange
   *  listener (it rebinds and unbinds it across retry attempts), and folding
   *  that owner into the conversation belongs to the axis work of the next
   *  grain. Only tests reach it today.
   */
  reattach(): void {
    this.restartWake?.();
  }

  /** Reset the restart backoff: a live stream means the next interruption
   *  starts at the first wait again. */
  protected resetRestartBackoff(): void {
    this.restartDelayMs = RESTART_MIN_MS;
  }

  /** Hook: a transport now drives the read path (the first attach and every
   *  rebuild). Subclasses wire their stream lifecycle here. */
  protected onStreamAttached(): void {}

  /** Hook for relay-only cleanup when the drain dies (abandon pending sends). */
  protected onDrainDead(): void {}
}

// ---------------------------------------------------------------------------
// RelayConversation (online)
// ---------------------------------------------------------------------------

/** Fire a system notification for an inbound authored message. The guard
 *  chain (window visibility, the user's settings switch, the platform
 *  permission) lives in notify(); this wrapper owns the 1:1 gates: the
 *  mounted-page view gate (same semantics as the rooms side) and the
 *  content gate: authored content frames only -- markers/acks/errors are
 *  not notify-worthy. The tag is the conversation key, so a burst
 *  collapses onto one notification (WHATWG replacement / grouping). */
function maybeNotifyBackground(
  tagmaId: string,
  label: string | null,
  reply: TagmaReply,
): void {
  if (reply.kind !== "event") return;
  if (reply.event.type !== "assistant_content") return;
  // Same viewing gate as the rooms side: a mounted conversation page is
  // already reading (its badge is cleared), so a notification here would
  // contradict the zeroed badge. document.hidden still applies inside
  // notify(); this check only silences the viewed-conversation case.
  if (unreadStore.isViewing(tagmaKey(tagmaId))) return;
  void notify({
    tag: tagmaKey(tagmaId),
    title: label ? `Tagma ${label}` : "Tagma",
    body: reply.event.content,
  });
}

/** The background-notification gate for the relay leaf: only a frame
 * stamped above the open-time high-water is new since this device last
 * looked. This is the id test alone: maybeNotifyBackground narrows to
 * event frames of type assistant_content, so echoes, markers, acks and
 * errors never reach notify(). */
export function shouldNotify(floor: number, reply: TagmaReply): boolean {
  return contentId(reply) > floor;
}

/** One in-flight relay history pull, keyed by `req_id` (the marker echoes
 * it). `rows` buffers the content frames the drain reroutes into this pull
 * -- the catch-up window or a scroll-up before-page (see
 * `RelayConversation.applyReplyCore`); `settle` resolves the pull's await
 * when the batch_end marker or the pull timeout lands. */
interface RelayPull {
  rows: HistoryEntry[];
  settle: ((count: number, more: boolean, undecodable: number) => void) | null;
}

/** One cursor page's request: `before` is the window floor a scroll-up
 *  pages below (absent/null for the catch-up window, which asks for the
 *  newest rows). */
interface PageRequest {
  before?: number | null;
  limit: number;
}

/** What a page pull settles with: its buffered rows, the marker's count
 *  and `more`, and whether the marker ever arrived. A timed-out page holds
 *  rows that may be partial, so callers keep their retry state armed. The
 *  `undecodable` count says whether the marker's window was whole. */
interface PageResult {
  rows: HistoryEntry[];
  count: number;
  more: boolean;
  timedOut: boolean;
  /** Rows the window held that the tagma could not decode: the batch is
   *  then a hole, not a complete window. Zero when the marker never came
   *  (a timed-out page makes no claim either way). */
  undecodable: number;
}

export class RelayConversation extends ConversationBase {
  readonly kind = "relay" as const;
  /** Pending rows partition by tagma (the tagma id is known offline). */
  get pendingKey(): string {
    return this.tagmaId;
  }

  /** The relay's write path is the channel itself: there is no separate
   *  carrier that outlives it, so the path exists exactly while a transport is
   *  attached and the channel is open. The two are one fact, and this is where
   *  that fact is stated: composing both into the write axis puts the
   *  guarantee in the definition, so no order of assignments in run()'s
   *  terminal branch can open a window. */
  protected override get carrierLive(): boolean {
    return this.transport !== null && this.status === "open";
  }

  readonly tagmaId: string;
  readonly label: string | null;

  /** Per-batch marker timeout. The server deliberately omits the marker on
   * partial delivery (dispatch.rs), so every pull carries its own deadline:
   * one slow batch is never condemned by an earlier batch's timer (a
   * whole-catch-up watchdog once force-flipped notifications mid-replay).
   * Public and mutable as the test seam (10s in production). */
  pullTimeoutMs = 10_000;

  /** In-flight history pulls by req_id: the catch-up window pull and a
   *  scroll-up before-page each resolve on THEIR OWN marker (the window pull
   *  doubles as the catch-up single-flight guard). */
  private pendingPulls = new Map<number, RelayPull>();
  /** The one in-flight before-page pull (or null). Replay rows at or below
   * the rendered cursor reroute here instead of being dedup-dropped. At
   * most one: loadOlder single-flights. */
  private beforePull: RelayPull | null = null;
  /** The one in-flight catch-up window pull (or null). Every content frame it
   *  brings lands in this buffer and folds as ONE batch; a send echo is the
   *  exception (it belongs to the send pump). Also the catch-up single-flight
   *  guard. */
  private windowPull: RelayPull | null = null;
  /** The notification floor: content frames at or below it are replay
   * (backlog being re-pulled) and never notify; above it is genuinely
   * new since this device last looked. Replaces the old live/watchdog
   * gate with an id test — no marker arrival required, so the gate can
   * never wedge. Set to MAX_SAFE_INTEGER for the whole catch-up replay
   * (suppress everything, batch rows included), reset to the live edge
   * (the final maxRowId) when catch-up ends. */
  private notifyFloor = 0;

  /** How the client reports a read cursor: a manage PUT on this
   *  conversation's own channel. Public as the test seam -- the fake channel
   *  in relayWindow_test has no manage plane. */
  readCursorSender: (cursor: number) => Promise<void> = (cursor) =>
    this.sendReadCursor(cursor);

  /** The highest id already reported by this instance (monotonic). */
  private lastReportedCursor = 0;
  /** The cursor a send has in flight: a second report of the same id
   *  before the first lands would send the same PUT twice, so the guard
   *  treats it as reported already. Only the write that set the marker
   *  clears it, so two overlapping writes cannot drop each other's; an
   *  overlap costs at most one redundant PUT, which the monotonic store
   *  absorbs. */
  private inFlightCursor = 0;
  /** The cursor waiting inside the throttle window: a busy chat lands one
   *  PUT per `PUT_THROTTLE_MS`, not one per line. 0 = nothing pending. */
  private pendingCursor = 0;
  /** The window timer seam: tests run the window without waiting it out. */
  readCursorTimer: (fn: () => void) => ReturnType<typeof setTimeout> = (fn) =>
    setTimeout(fn, PUT_THROTTLE_MS);
  private cursorTimer: ReturnType<typeof setTimeout> | null = null;
  /** Whether a throttle window is open. The flag, not the timer handle, is the
   *  window's state: a seam that fires the callback synchronously would leave
   *  a handle behind that a later report could mistake for an open window. */
  private cursorWindowOpen = false;

  constructor(
    conversationId: string,
    store: ConversationStoreLike,
    transport: Transport,
    tagmaId: string,
    label: string | null,
    /** The write path's carrier (the relay channel's op). */
    sender: MessageSender | null = null,
  ) {
    // Relay store key == the derived conversation id == the cache key.
    super(
      conversationId,
      store,
      transport,
      conversationId,
      transport.localSender,
      sender,
    );
    this.tagmaId = tagmaId;
    this.label = label;
    // The relay channel is the sender's carrier; a caller may pass its own.
    if (this.sender === null) {
      this.sender = new RelayMessageSender(this.relayTransport.relayChannel);
    }
  }

  /** The E2EE transport (for the store's history pull, envelope delivery,
   * and signal routing). */
  get relayTransport(): import("./relayTransport.ts").RelayTransport {
    return this.transport as import("./relayTransport.ts").RelayTransport;
  }

  /** Test seam: deliver one decrypted frame as the drain would. The fake
   * channel in relayWindow_test bypasses the real E2EE pipe, so this is
   * the entry point its frames use. */
  feed(reply: TagmaReply, sender: Participant | undefined): void {
    this.applyReplyCore(reply, sender);
  }

  /** The conversation page mounted: tell the tagma how far this device has
   *  read, so it stamps the badge empty. Rows landing while the page stays
   *  mounted move the position cursor, which reports through
   *  {@link reportPosition}; a lost report self-heals on the next row or the
   *  next mount, because the cursor is monotonic server-side. */
  markRead(): void {
    this.reportReadCursor(this.lastSeq);
  }

  /** The relay carries the read-cursor write; the base calls this whenever a
   *  rendered position advances the cursor. */
  protected override reportPosition(cursor: number): void {
    this.reportReadCursor(cursor);
  }

  /** Report a read cursor, page mounted only: a conversation nobody is
   *  looking at must never be marked read (that is what the tagma's own
   *  stamps are for). Reports coalesce into one PUT per `PUT_THROTTLE_MS`
   *  window; {@link flushReadCursor} lands the window early (page unmount).
   *
   *  Only a landed write advances the reported watermark, so a failed one keeps
   *  its retry point: the pending cursor survives and the next line, or this
   *  page's next mount, reports it again (the cursor is monotonic in the store,
   *  so a repeat is harmless). */
  private reportReadCursor(cursor: number): void {
    if (!Number.isSafeInteger(cursor) || cursor <= this.lastReportedCursor) {
      return; // already reported, or not a durable id
    }
    if (!unreadStore.isViewing(tagmaKey(this.tagmaId))) return;
    this.pendingCursor = Math.max(this.pendingCursor, cursor);
    this.openCursorWindow();
  }

  /** Open the throttle window unless one is already open. */
  private openCursorWindow(): void {
    if (this.cursorWindowOpen) return;
    this.cursorWindowOpen = true;
    this.cursorTimer = this.readCursorTimer(() => this.flushReadCursor());
  }

  /** Land the pending cursor now: the window fired, or the page unmounted
   *  with a report still inside it. Nothing pending is a no-op. */
  flushReadCursor(): void {
    if (this.cursorTimer !== null) {
      clearTimeout(this.cursorTimer);
      this.cursorTimer = null;
    }
    this.cursorWindowOpen = false;
    const cursor = this.pendingCursor;
    if (cursor <= Math.max(this.lastReportedCursor, this.inFlightCursor)) {
      return;
    }
    this.inFlightCursor = Math.max(this.inFlightCursor, cursor);
    void this.readCursorSender(cursor).then(
      () => {
        this.lastReportedCursor = Math.max(this.lastReportedCursor, cursor);
        if (this.inFlightCursor === cursor) this.inFlightCursor = 0;
      },
      () => {
        // Best-effort: the pending cursor stays for the next attempt.
        if (this.inFlightCursor === cursor) this.inFlightCursor = 0;
      },
    );
  }

  /** The production sender: the tagma scopes the cursor to the peer this
   *  channel authenticated, so only the id itself crosses the wire. The reply
   *  is a bare `204`, so the status class is the success judgement and the
   *  resulting count arrives as an unread frame like any other. */
  private async sendReadCursor(cursor: number): Promise<void> {
    const channel = this.relayTransport.relayChannel;
    const root = await relayRootAgentId(channel);
    if (root === null) {
      warnRootlessOnce(channel);
      throw new Error("root agent id unresolved");
    }
    const res = await channel.manage(
      "PUT",
      `/agents/${root}/external/read-cursor`,
      { cursor },
    );
    if (!manageOk(res)) {
      throw new Error(`read cursor rejected: ${res.status}`);
    }
  }

  /** Reroute replay rows into the in-flight before-page. The core's dedup
   * floors on minRowId (rows below it were rendered and let go), so a
   * scroll-up page's older rows would vanish; worse, a user_message whose
   * text matches the in-flight send would be mis-consumed as its echo,
   * promoting the optimistic line to a duplicate old id. Genuine echoes
   * never take this branch: a relay echo is stamped above the cursor, a
   * direct-path echo carries history_id 0, and acks/markers carry no
   * content. */
  protected override applyReplyCore(
    reply: TagmaReply,
    sender: Participant | undefined,
  ): void {
    const id = contentId(reply);
    const page = this.beforePull;
    if (page && sender && id > 0 && id <= this.maxRowId) {
      page.rows.push({ sender, reply });
      return;
    }
    // The catch-up window takes every content frame as one batch. The one
    // exception is a send echo: it belongs to the send pump (it promotes the
    // optimistic line), and folding it as a page row would render it twice.
    if (this.windowPull && sender && id > 0 && !this.isInFlightEcho(reply)) {
      this.windowPull.rows.push({ sender, reply });
      return;
    }
    super.applyReplyCore(reply, sender);
  }

  /** Whether this frame answers the send currently in flight (the core
   *  correlates by text: an echo carries no req_id). */
  private isInFlightEcho(reply: TagmaReply): boolean {
    return (
      reply.kind === "user_message" &&
      this.pendingInFlight !== null &&
      reply.text.trim() === this.pendingInFlight.text.trim()
    );
  }

  protected override onReply(reply: TagmaReply): void {
    if (reply.kind === "history_batch_end") {
      // The tagma's count travels with the marker: assign it, never count the
      // rows this batch brought. The wire's stamp is one object -- count and
      // cursor arrive as a pair -- so a tagma that predates the field sends
      // neither, and an absent stamp must not overwrite one that arrived by
      // another carrier.
      if (reply.unread !== undefined) {
        unreadStore.setTagmaUnread(this.tagmaId, {
          unread: reply.unread,
          cursor: reply.cursor,
        });
      }
      const pull = this.pendingPulls.get(reply.req_id);
      if (pull) {
        this.pendingPulls.delete(reply.req_id);
        if (this.beforePull === pull) this.beforePull = null;
        pull.settle?.(reply.count, reply.more, reply.undecodable ?? 0);
      }
      // Close the window bracket: drain-side batches advance only
      // maxRowId, so the scroll sentinel needs minRowId recomputed
      // once the batch has landed (idempotent; before-pages recompute
      // again through the merge).
      this.recomputeMinRendered();
      return;
    }
    if (shouldNotify(this.notifyFloor, reply)) {
      maybeNotifyBackground(this.tagmaId, this.label, reply);
    }
  }

  /** Adopt the newest window: ONE pull, ONE fold, ONE badge derivation.
   *
   *  The window is the unit: one request covers the newest rows, older rows
   *  load on scroll-up (what loadOlder already pages), and the badge is a
   *  single derivation from the window, not a running tally of rows arriving
   *  one by one.
   *
   *  The pull buffers its content frames and folds them through
   *  applyPulledRows (one transcript update, one cache pass); the badge is
   *  derived from the same rows in one store update.
   */
  override async catchUp(k = WINDOW_PAGE): Promise<void> {
    if (this.windowPull !== null) return; // one window at a time
    // Content frames carry no request id: a window pull and a before-page
    // cannot be in flight together (see runPagePull). Waiting on the queue
    // here is best-effort -- it is a snapshot of its tail, not a lock: a page
    // queued after this await still races, and the recheck inside runPagePull
    // is what actually keeps the two apart.
    await this.beforeQueue;
    if (this.windowPull !== null) return;
    // Suppress notifications for the whole window: every row in it is old news
    // re-arriving, and a fresh device's window starts below no floor at all
    // (maxRowId 0) -- either way one notification per row would be spam.
    // The floor resets to the live edge (the final maxRowId) when the window
    // lands, so only frames newer than it can ever notify.
    this.notifyFloor = Number.MAX_SAFE_INTEGER;
    try {
      const page = await this.pullPage({ limit: k });
      this.adoptWindow(page.rows);
      // A window short of k RAW rows (decoded plus dropped) IS all the history
      // there is; a timed-out one proves nothing below and keeps the sentinel.
      const rawRows = page.count + page.undecodable;
      if (!page.timedOut && rawRows < k) this.hasMoreOlder = false;
    } catch {
      // Dead channel: the drain surfaces transport status; the window stays on
      // whatever the hydrate landed and live frames keep arriving.
    } finally {
      // Marker or not, whatever landed forms the window bracket (covers a
      // timed-out window that streamed rows but no marker).
      this.recomputeMinRendered();
      this.notifyFloor = this.maxRowId;
    }
  }

  /** Fold an adopted window: drop rendered rows it does not reach, then fold
   *  the whole batch in one pass. The badge is untouched: it arrives stamped
   *  from the tagma, so adopting rows -- a cache rehydrate, a replay, a stale
   *  page -- can never move it. */
  private adoptWindow(rows: HistoryEntry[]): void {
    if (rows.length === 0) return;
    // head comes from CONTENT rows only. The window buffer is fed by a
    // content-id guard, but the bracket must not rest on that: one zero would
    // silently disarm the continuity test, so filter here and bail instead
    // of trusting the feeder.
    const ids = rows.map((e) => contentId(e.reply)).filter((id) => id > 0);
    if (ids.length === 0) return;
    const head = Math.min(...ids);
    this.pruneRenderedBefore(head);
    this.applyPulledRows(rows);
    // The window is what this device has now read: folding its rows moved
    // the position cursor (applyPulledRows), so a conversation opened with
    // unread rows clears on the server too, not only through the badge the
    // `enter` zeroed locally.
  }

  /** Drop rendered durable rows that the adopted window does not reach. They
   *  stay in the cache, and the paged path brings them back in order; keeping
   *  them rendered would put the old cache tail beside the new window with an
   *  unknown gap between the two -- a hole no page can fill, because paging
   *  always continues BELOW the current window head. Rows that touch or
   *  overlap the window (`max >= head - 1`) are kept: they are continuous.
   *  Optimistic and synthetic lines (non-positive ids) are never touched. */
  private pruneRenderedBefore(headId: number): void {
    const durable = this.transcript.lines.filter((l) => l.historyId > 0);
    if (durable.length === 0) return;
    const max = Math.max(...durable.map((l) => l.historyId));
    if (max >= headId - 1) return;
    this.transcript = {
      ...this.transcript,
      lines: this.transcript.lines.filter((l) => l.historyId <= 0),
    };
  }

  /** The relay leaf's page source: the cache when the cached segment actually
   *  abuts the window head, then an encrypted before-page below the window
   *  floor. A timed-out page (lost marker) folds its buffered rows and leaves
   *  the sentinel armed -- the next scroll retries.
   *
   *  The adjacency test is what keeps the transcript continuous: the cache can
   *  hold a segment the adopted window does not reach (a long gap, a capped
   *  history), and splicing that in would render an unknown hole above it that
   *  paging can never fill. A non-adjacent cache page is skipped; the server
   *  page is contiguous by construction (`before` = the newest rows below the
   *  head).
   */
  protected override async loadOlderPage(k: number): Promise<void> {
    // A catch-up window owns the drain's content frames while it is in flight:
    // paging now would divide them between the two buffers. The sentinel stays
    // armed, so the next scroll retries.
    if (this.windowPull !== null) return;
    const cached = await this.readCachedPage(k);
    if (cached.length > 0) {
      const cachedTop = cached[cached.length - 1]!.historyId;
      if (cachedTop >= this.minRowId - 1) {
        this.mergeWindowLines(cached.map(cachedLineToLine));
        if (cached.length >= k) return;
      }
    }
    // Page below the window floor as it stands NOW: when a cached page was
    // merged above, this continues beneath it instead of re-fetching the rows
    // just rendered (a re-fetch folds to `added === 0` and would disarm the
    // sentinel with history still below).
    const { rows, more, timedOut } = await this.pullPage({
      before: this.minRowId,
      limit: k,
    });
    const added = this.applyPulledRows(rows);
    // added === 0 disarms for a different reason than !more: a page whose rows
    // all failed to decode cannot move the cursor, so re-paging returns it again.
    if (!timedOut && (!more || added === 0)) this.hasMoreOlder = false;
  }

  /** The cache page below the window head. A seam so the adjacency rule is
   *  testable without IndexedDB (the production read is the shared per-tagma
   *  cache -- the same key the offline entry uses). */
  protected readCachedPage(k: number): Promise<CachedLine[]> {
    return readTailBefore(this.cacheConversationId, this.minRowId, k);
  }

  /** Issue one cursor page and await ITS batch_end marker
   * (req_id-correlated; the rows stream through the drain meanwhile). The
   * per-batch timeout is the old watchdog's replacement: on expiry the
   * buffered rows fold, the page reads as terminal, and retry policy
   * stays with the caller. */
  private async pullPage(opts: PageRequest): Promise<PageResult> {
    // A dead transport has no channel to page through: fold as a timed-out
    // empty page so the caller keeps hasMoreOlder instead of reading the
    // miss as a terminal end of history. A closed conversation pages nothing
    // either: the request would only ever pay its timeout.
    if (this.transport === null || this.closed) {
      return { rows: [], count: 0, more: true, timedOut: true, undecodable: 0 };
    }
    const pull: RelayPull = { rows: [], settle: null };
    const isBeforePage = (opts.before ?? 0) > 0;
    const run = () => this.runPagePull(pull, isBeforePage, opts);
    // Before-pages queue: one in flight at a time (see withBeforeSlot).
    return isBeforePage ? await this.withBeforeSlot(run) : await run();
  }

  /** One before-page in flight at a time. The wire's content frames carry no
   *  request id (only the batch-end marker does), so a row belongs to "the
   *  before-page in flight" and to nothing finer. Two of them would trade
   *  rows: the earlier one settles with an empty, non-timed-out page (its
   *  quote target then reads as deleted) and the scroll-up loses its page
   *  (added === 0 disarms the sentinel). Before-pages queue here; the
   *  catch-up window keeps its own slot. */
  private beforeQueue: Promise<unknown> = Promise.resolve();

  private async withBeforeSlot<T>(run: () => Promise<T>): Promise<T> {
    const previous = this.beforeQueue;
    let release!: () => void;
    this.beforeQueue = new Promise<void>((resolve) => {
      release = resolve;
    });
    await previous.then(
      () => undefined,
      () => undefined,
    );
    try {
      return await run();
    } finally {
      release();
    }
  }

  /** Register this page's buffer, issue the request, and settle on its own
   *  marker. Reached through {@link pullPage}, which serializes before-pages. */
  private async runPagePull(
    pull: RelayPull,
    isBeforePage: boolean,
    opts: PageRequest,
  ): Promise<PageResult> {
    // The try covers the history() call too: if the channel rejects the
    // send, the finally still drops this pull off beforePull — an orphan
    // reroute target would otherwise swallow rows into a dead page.
    // The sibling slot, re-checked at the point of execution rather than
    // once before the queue: a before-page can wait in beforeQueue while the
    // catch-up window starts, and the wire cannot tell the two apart (content
    // frames carry no request id). A page that ran anyway would swallow the
    // window's replayed rows; the window would then fold around an empty page.
    // Idle out as a timed-out page instead: scroll paging keeps its sentinel
    // armed, and the quote probe reads an error, never a deleted target.
    if (isBeforePage && this.windowPull !== null) {
      return { rows: [], count: 0, more: true, timedOut: true, undecodable: 0 };
    }
    let req_id = -1;
    try {
      // Register the buffer BEFORE the request: the rows ride the drain and can
      // land while history() is still in flight.
      if (isBeforePage) this.beforePull = pull;
      else this.windowPull = pull;
      req_id = await this.relayTransport.relayChannel.history(opts);
      this.pendingPulls.set(req_id, pull);
      return await new Promise((resolve) => {
        const timer = setTimeout(() => {
          // Partial delivery (the server stops without the marker):
          // resolve with what buffered; `timedOut` tells callers not
          // to trust `more`.
          resolve({
            rows: pull.rows,
            count: pull.rows.length,
            more: false,
            timedOut: true,
            undecodable: 0,
          });
        }, this.pullTimeoutMs);
        pull.settle = (count, more, undecodable) => {
          clearTimeout(timer);
          resolve({
            rows: pull.rows,
            count,
            more,
            timedOut: false,
            undecodable,
          });
        };
      });
    } finally {
      if (req_id >= 0) this.pendingPulls.delete(req_id);
      if (isBeforePage && this.beforePull === pull) {
        this.beforePull = null;
      }
      if (!isBeforePage && this.windowPull === pull) this.windowPull = null;
    }
  }

  /** The relay leaf's probe page: one before-page below the id, read and
   *  not merged -- the transcript's window stays exactly as the reader left
   *  it. A before-page returns the newest rows at or below the cursor, which
   *  is what lets the base read a contiguous miss as `deleted`. */
  protected async probePage(id: number): Promise<HistoryEntry[] | null> {
    // Three guards, one per way the answer is unusable: no transport, a closed
    // conversation, and a live catch-up window that owns the drain's content
    // frames. `closed` also overlaps pullPage's fold (closed reads as timed
    // out, which reads as no answer), but that equivalence lives in another
    // method, so the short circuit stays here as defence in depth.
    if (this.transport === null || this.closed || this.windowPull !== null) {
      return null;
    }
    try {
      const { rows, count, timedOut, undecodable } = await this.pullPage({
        before: id + 1,
        limit: 1,
      });
      // A hole, or rows that never arrived, cannot prove a contiguous miss.
      return timedOut || undecodable > 0 || rows.length < count ? null : rows;
    } catch {
      return null;
    }
  }

  protected override onDrainDead(): void {
    this.abandonPending();
  }

  /** Drop all unsent optimistic state: the in-flight slot, the queued
   * entries, and their rendered "sending" lines. Used when the channel
   * dies. */
  abandonPending(): void {
    this.pending = [];
    this.pendingInFlight = null;
    this.clearWatchdog();
    this.transcript = {
      ...this.transcript,
      lines: this.transcript.lines.filter((l) => l.status !== "sending"),
    };
  }
}

// ---------------------------------------------------------------------------
// LocalConversation (offline)
// ---------------------------------------------------------------------------

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
    this.status = "open";
    this.restartStream = restartStream;
    this.onStreamAttached();
  }

  /** The direct history pull, taken from the sender: a dead stream cannot take
   *  the backfill with it. Null when this conversation has no puller. */
  private get pull(): ((opts: PullOpts) => Promise<HistoryPage>) | null {
    const sender = this.sender;
    const pull = sender?.pullHistory;
    return pull === undefined ? null : pull.bind(sender);
  }

  /** Wire the stream's retry lifecycle for whichever transport drives the read
   *  path right now -- the first attach and every rebuild. A `missed` frame
   *  carries no position: this device's own cursor says where to resume, so
   *  the repair starts there. */
  protected override onStreamAttached(): void {
    const t = this.transport;
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
      this.status = "reconnecting";
    } else {
      this.status = "open";
      this.resetRestartBackoff();
      // Back online: flush the durable unsent lines once (retrySend is
      // idempotent, so racing retries collapse).
      void this.retryAllPending();
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
    const head = this.minRowId;
    const below = this.minSeq;
    const cached = await readTailBefore(this.cacheConversationId, head, k);
    if (cached.length > 0) {
      this.mergeWindowLines(cached.map(cachedLineToLine));
    }
    if (cached.length >= k) return;
    const pull = this.pull;
    if (pull === null) return;
    // Below the window means below its oldest rendered position: a window
    // read takes a position, and a page's `next` is its own edge, not the
    // anchor the cache just extended.
    if (below <= 0) return;
    const { rows, more } = await pull({ beforeSeq: below, limit: k });
    const added = this.applyPulledRows(rows);
    // Same distinction as the relay leaf: !more reads the database, while
    // added === 0 reads this window -- all-dropped rows cannot move the cursor.
    if (!more || added === 0) this.hasMoreOlder = false;
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
    if (pull === null || this.status !== "open") return null;
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

/** The degraded offline view of a relay conversation: the key exchange
 * could not run (the tagma is unreachable), so there is no transport --
 * the transcript hydrates from the per-device cache, sends land in the
 * pending store for a later retry/auto-flush, and scroll-up pages come
 * from the cache alone. Registered under the mapped conversation id AND
 * carrying the tagma id, so a successful re-open sweeps it by tagma id
 * instead of coexisting with the live conversation. */
export class OfflineConversation extends ConversationBase {
  readonly kind = "offline-view" as const;

  constructor(
    conversationId: string,
    store: ConversationStoreLike,
    readonly tagmaId: string,
    localSender: ConversationSender,
  ) {
    // No transport: run() ends immediately. Pin "offline" here -- the base
    // default is "opening", which would lock the composer and gates forever.
    super(conversationId, store, null, conversationId, localSender);
    this.status = "offline";
  }

  get pendingKey(): string {
    return this.tagmaId;
  }
  /** Writable without sending anything now: the offline view's path is the
   *  pending store, which always takes a line (a durable row a later real
   *  channel flushes). So its carrier fact is unconditionally true, and only
   *  `closed`, composed by the base, stops it. */
  protected override get carrierLive(): boolean {
    return true;
  }

  /** The offline view pages its cache for scroll-up but holds no history
   *  source, so it cannot answer a probe. */
  protected probePage(): Promise<HistoryEntry[] | null> {
    return Promise.resolve(null);
  }

  /** The base send holds a sender for the conversation's life and fails the
   *  line with `error` once it is closed; the offline view has no sender at
   *  all, so it keeps its own send -- the line renders failed (retryable) and
   *  persists to the pending store for the reconnect auto-flush. No pump:
   *  there is nothing to pump into until a real channel opens. */
  override send(text: string, options?: SendOptions): void {
    if (!this.canWrite) {
      this.error = new Error("conversation closed");
      return;
    }
    const trimmed = text.trim();
    if (trimmed === "" && options?.attachment === undefined) return;
    const localId = this.renderPendingLine(trimmed, options);
    this.transcript = sendFailed(this.transcript, localId, chat_send_failed());
  }

  /** No retry on the offline view: with no transport the pump would
   *  throw inside its send and the catch would demote the durable
   *  queued row to failed, cancelling the reconnect auto-flush that
   *  send() deliberately preserves. The failed line keeps its retry
   *  button only where a live channel exists. */
  override retrySend(_localSeq: number): void {}
  /** Cache-first page source: offline there is nothing else. */
  protected override async loadOlderPage(k: number): Promise<void> {
    const head = this.minRowId;
    if (!head) return;
    const cached = await readTailBefore(this.cacheConversationId, head, k);
    this.mergeWindowLines(cached.map(cachedLineToLine));
    // The cache is the only source offline: a short page means it holds
    // nothing older, and no transport could have dropped rows.
    if (cached.length < k) this.hasMoreOlder = false;
  }
}
