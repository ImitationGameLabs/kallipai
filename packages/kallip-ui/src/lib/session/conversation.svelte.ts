// Per-conversation reactive state, drained from a {@link Transport}. One base
// holds the shared surface (transcript, transport status, the two-stream drain,
// the single-in-flight send pump, the history cursor + cache + dedup, and the
// optimistic-line promotion), and the relay leaf carries its transport-specific
// extras (req_id-correlated cursor pulls, the before-page reroute, and the
// background-notification gate):
//
//   - `RelayConversation` (online): the E2EE pipe; history pages are cursor
//     pulls that await their batch_end marker, replayed rows below the
//     rendered cursor reroute into the pending page, and only frames above
//     the open-time high-water fire background notifications.
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
  EMPTY_TRANSCRIPT,
  historyEntryLine,
  isInFlightError,
  markLineSent,
  mergeHistoryLines,
  nextPendingSeq,
  replaceLineId,
  retryLine,
  sendFailed,
  toSender,
  withUserLine,
} from "../transcript.ts";
import type {
  ConversationLine,
  ConversationSender,
  ConversationTranscript,
} from "../transcript.ts";
import type {
  CachedLine,
  FileAttachment,
  HistoryEntry,
  Participant,
  TagmaReply,
} from "@kallipai/kallip-lesche-client";
import {
  deletePending,
  isFailedPending,
  markPendingFailed,
  type PendingLine,
  put as cachePut,
  putPending,
  readPendingByTagma,
  readTailBefore,
} from "@kallipai/kallip-lesche-client";
import type { Transport } from "./transport.ts";
import { DirectTransport, type TransportState } from "./directTransport.ts";
import { LescheApiError } from "@kallipai/kallip-lesche-client";
import { KallipError } from "@kallipai/kallip-common";
import {
  chat_send_failed,
  chat_send_timed_out,
} from "../../paraglide/messages.js";

import { tagmaKey, unreadStore } from "./unread.svelte.ts";
import { statusCardStore } from "./statusCard.svelte.ts";
import { notify } from "./notify.ts";

/** How long an accepted send waits for its ack/error reply before the
 * watchdog presumes the reply lost. Aligned with the SSE connect timeout
 * and the per-attempt fetch cap (30s each). */
const SEND_WATCHDOG_MS = 30_000;

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
  };
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

  /** The underlying transport, or null once the drain has ended. `$state` so
   *  `connected` (and the store's `localConnected`) re-fire when run() nulls it
   *  on a clean/error drain end -- not just on attach/detach. */
  protected transport = $state<Transport | null>(null);

  /** The IndexedDB cache key (the tagma's conversation id). The offline
   *  `"local"` entry and the online derived-id entry for the same tagma share
   *  this so a mode switch rehydrates from the same rows. Falls back to the
   *  store key when the tagma is not enrolled (no durable history). */
  readonly cacheConversationId: string;
  /** The largest confirmed history_id rendered (the dedup cursor). Any inbound
   *  frame with id <= this is dropped -- unifies catch-up batch and live. */
  maxRendered = $state(0);
  /** Rendered optimistic user lines awaiting their POST. Each entry's line is
   *  already in `transcript` (status "sending"); the single-in-flight send pump
   *  drains this one ack at a time. */
  pending = $state<
    { localId: number; text: string; attachment?: FileAttachment }[]
  >([]);

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
   *  `0` = nothing loaded. Together with `maxRendered` it brackets what
   *  the transcript currently holds; rows outside the bracket are fetched
   *  on demand (catch-up above, scroll-paging below), never up front. */
  minRendered = $state(0);
  /** True while a scroll-up page is in flight (single-flight guard + the
   *  top sentinel's busy state). */
  loadingOlder = $state(false);
  /** False once an older page came back empty — the sentinel stops
   *  arming. Stays true while unknown (conservative: an extra empty
   *  pull is cheap, a missed page is not). */
  hasMoreOlder = $state(true);

  constructor(
    readonly conversationId: string,
    protected readonly store: ConversationStoreLike,
    transport: Transport | null,
    cacheConversationId: string,
    /** The UI sender for optimistic user lines (online: the archeion session;
     *  offline: the tagma-configured local identity). */
    protected readonly localSender: ConversationSender,
  ) {
    this.transport = transport;
    this.cacheConversationId = cacheConversationId;
  }

  get connected(): boolean {
    return this.transport !== null;
  }

  /** Tear down the underlying transport. The run() drain then ends; its stale
   *  guard stops it from mutating the (likely-removed) conversation. */
  close(): void {
    this.transport?.close();
  }
  /** Send a user message online: renders the optimistic line, queues it,
   *  and hands off to the shared single-in-flight send pump (the in-flight
   *  POST's `user_message` frame promotes the line via `applyReplyCore`). */
  send(text: string, attachment?: FileAttachment): void {
    if (!this.transport) return;
    const trimmed = text.trim();
    if (trimmed === "" && attachment === undefined) return;
    const localId = this.renderPendingLine(trimmed, attachment);
    this.pending = [...this.pending, { localId, text: trimmed, attachment }];
    void this.pumpPending();
  }

  /** Render + persist one locally authored line: the shared core of the
   *  online send and the offline shell's send. The caller trims and
   *  empty-gates the text; this takes the next localSeq, appends the
   *  optimistic bubble, and writes the durable pending row (removed when
   *  the ack lands). Returns the localSeq, which doubles as the rendered
   *  line's historyId. */
  protected renderPendingLine(
    trimmed: string,
    attachment?: FileAttachment,
  ): number {
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
    );
    void putPending({
      tagmaId: this.pendingKey,
      localSeq: localId,
      text: trimmed,
      ...(attachment !== undefined ? { attachment } : {}),
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
      { localId: localSeq, text: line.text, attachment: line.attachment },
    ];
    void this.pumpPending();
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
        { localId: row.localSeq, text: row.text, attachment: row.attachment },
      ];
    }
    if (this.pending.length > 0) void this.pumpPending();
  }

  /** Hook for leaves to react to a reply AFTER the shared core reduce (the relay
   *  flips `live` on `history_batch_end` and fires background notifications). */
  protected onReply(_reply: TagmaReply): void {}

  /** Line-entry hook: one NEW line landed through the reducer (a live frame
   *  or a catch-up row, post-dedup). `realId` is the durable id (0 for a
   *  synthetic line). The relay leaf counts unread from here so both paths
   *  pass the same point; the base no-op keeps the offline leaf unhooked. */
  protected onLineLanded(_reply: TagmaReply, _realId: number): void {}

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
        const confirmed = this.transcript.lines.find(
          (l) => l.historyId === ackId,
        );
        if (confirmed) {
          // Cache the confirmed user line. Use the plain `wireSender` -- NOT
          // `confirmed.sender`: the line lives in a `$state` transcript, so its
          // nested `sender` is a Svelte Proxy, which IndexedDB's structured clone
          // rejects (DataCloneError). `put` swallows that error, so reading the
          // proxy sender here silently dropped EVERY user message from the cache
          // (they vanished on refresh). `wireSender` is a fresh plain object.
          // `text`/`createdAt` are primitives, safe to read off the proxy line.
          void cachePut({
            conversationId: this.cacheConversationId,
            historyId: ackId,
            role: "user",
            text: confirmed.text,
            sender: wireSender,
            createdAt: confirmed.createdAt,
            attachment: confirmed.attachment,
          });
        }
        // The send landed: the durable pending copy is no longer needed.
        void deletePending(this.pendingKey, localId);
        if (ackId > this.maxRendered) this.maxRendered = ackId;
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
    // Dedup: a frame at or below the cursor is a replay of something rendered.
    const realId =
      reply.kind === "event" || reply.kind === "user_message"
        ? (reply.history_id ?? 0)
        : 0;
    if (realId > 0 && realId <= this.maxRendered) {
      this.onReply(reply);
      return;
    }
    const lineId = realId > 0 ? realId : (this.syntheticSeq -= 1);
    this.transcript = applyTagmaReply(this.transcript, reply, sender, lineId);
    this.onLineLanded(reply, realId);
    const cl = cacheLineOf(reply, sender);
    if (cl) {
      void cachePut({
        conversationId: this.cacheConversationId,
        historyId: cl.historyId,
        role: cl.role,
        text: cl.text,
        sender: cl.sender,
        createdAt: cl.createdAt,
        attachment: cl.attachment,
      });
      if (cl.historyId > this.maxRendered) this.maxRendered = cl.historyId;
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
    const next = this.pending.shift();
    if (next === undefined) return;
    this.pendingInFlight = { localId: next.localId, text: next.text };
    try {
      const reqId = await this.transport!.send(next.text, next.attachment);
      // The POST was accepted: arm the reply watchdog. From here the only
      // way the line resolves is the ack or an error reply arriving on the
      // wire -- if neither lands (reply lost), the watchdog releases the
      // pump and fails the line for an explicit retry.
      this.armWatchdog(next.localId);
      // The channel stamps this send's req_id on accept; the reply-side
      // error correlation (applyReplyCore) closes the line on it.
      if (this.pendingInFlight) {
        this.pendingInFlight = {
          ...this.pendingInFlight,
          reqId: reqId ?? undefined,
        };
      }
    } catch (e) {
      // Typed failures (direct KallipError / relay LescheApiError) keep
      // their server copy; transport failures are qualitative.
      let failureCopy: string;
      if (e instanceof KallipError || e instanceof LescheApiError) {
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
   *  transcript; both window cursors recompute from the merged lines.
   *  Returns how many rows were newly added (0 = the page was fully
   *  redundant — the idempotency signal loadOlder uses to stop paging). */
  protected mergeWindowLines(rows: ConversationLine[]): number {
    const { transcript, added } = mergeHistoryLines(this.transcript, rows);
    if (added === 0) return 0;
    this.transcript = transcript;
    let min = Infinity;
    let max = 0;
    for (const l of transcript.lines) {
      if (l.historyId > 0) {
        if (l.historyId < min) min = l.historyId;
        if (l.historyId > max) max = l.historyId;
      }
    }
    if (min !== Infinity) this.minRendered = min;
    if (max > this.maxRendered) this.maxRendered = max;
    return added;
  }

  /** Ingest one pulled history batch: persist every row to the cache first
   *  (the put is historyId-keyed, so re-pulls are idempotent), then fold
   *  the rows into the window. Gap catch-up and scroll paging both land
   *  here — the pulled path must cache what it renders, because unlike the
   *  live drain it bypasses applyReplyCore's cachePut. */
  protected applyPulledRows(rows: HistoryEntry[]): number {
    const lines = rows
      .map(historyEntryLine)
      .filter((l): l is ConversationLine => l !== null);
    for (const l of lines) {
      void cachePut({
        conversationId: this.cacheConversationId,
        historyId: l.historyId,
        role: l.role,
        text: l.text,
        sender: l.sender,
        createdAt: l.createdAt,
      });
    }
    return this.mergeWindowLines(lines);
  }
  /** Recompute `minRendered` from the rendered durable lines. The
   * drain-side batch path appends rows through applyReplyCore (which
   * advances only maxRendered); this closes the window bracket once a
   * batch has landed so the scroll sentinel can arm. Idempotent,
   * O(window). */
  protected recomputeMinRendered(): void {
    let min = Infinity;
    for (const l of this.transcript.lines) {
      if (l.historyId > 0 && l.historyId < min) min = l.historyId;
    }
    if (min !== Infinity) this.minRendered = min;
  }

  /** Fetch one page older than the window head. Template method: the
   * shared guards, the single-flight flag, and the quiet error handling
   * live here; the leaf supplies the page source via loadOlderPage.
   *
   *  minRendered === 0 means the window has not formed yet (hydrate or
   *  catch-up still in flight, or the server truly has nothing): paging
   *  older-than-nothing is ill-defined — and racing the initial fill here
   *  once issued a recent-N pull whose always-false `more` permanently
   *  disarmed the sentinel. Wait for a window head to exist. */
  async loadOlder(k = WINDOW_PAGE): Promise<void> {
    if (this.loadingOlder || !this.hasMoreOlder) return;
    if (this.minRendered <= 0) return;
    this.loadingOlder = true;
    try {
      await this.loadOlderPage(k);
    } catch {
      // Offline / dead transport: retry on the next scroll-to-top.
    } finally {
      this.loadingOlder = false;
    }
  }

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

  /** Drain all three transport streams concurrently. Sets status to
   *  error/offline when the transport ends, guarded against a stale drain. */
  async run(): Promise<void> {
    const t = this.transport;
    if (!t) return;
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
    this.onDrainDead();
    this.transport = null;
  }

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

/** The background-notification gate for the relay leaf: only a content
 * frame stamped above the open-time high-water is new since this device
 * last looked. Markers/acks/errors never notify (not authored content). */
export function shouldNotify(floor: number, reply: TagmaReply): boolean {
  const id =
    reply.kind === "event" || reply.kind === "user_message"
      ? (reply.history_id ?? 0)
      : 0;
  return id > floor;
}

/** One in-flight relay history pull, keyed by `req_id` (the marker echoes
 * it). `rows` buffers frames the drain rerouted into a before-page (see
 * `RelayConversation.applyReplyCore`); `settle` resolves the pull's await
 * when the batch_end marker — or the pull timeout — lands. */
interface RelayPull {
  rows: HistoryEntry[];
  settle: ((count: number, more: boolean) => void) | null;
}

export class RelayConversation extends ConversationBase {
  readonly kind = "relay" as const;
  /** Pending rows partition by tagma (the tagma id is known offline). */
  get pendingKey(): string {
    return this.tagmaId;
  }

  readonly tagmaId: string;
  readonly label: string | null;

  /** Per-batch marker timeout. The server deliberately omits the marker on
   * partial delivery (dispatch.rs), so every pull carries its own deadline:
   * one slow batch is never condemned by an earlier batch's timer (a
   * whole-catch-up watchdog once force-flipped notifications mid-replay).
   * Public and mutable as the test seam (10s in production). */
  pullTimeoutMs = 10_000;

  /** In-flight history pulls by req_id. Catch-up after-pages and a
   * scroll-up before-page can overlap; each await resolves on ITS OWN
   * marker. */
  private pendingPulls = new Map<number, RelayPull>();
  /** The one in-flight before-page pull (or null). Replay rows at or below
   * the rendered cursor reroute here instead of being dedup-dropped. At
   * most one: loadOlder single-flights. */
  private beforePull: RelayPull | null = null;
  /** The notification floor: content frames at or below it are replay
   * (backlog being re-pulled) and never notify; above it is genuinely
   * new since this device last looked. Replaces the old live/watchdog
   * gate with an id test — no marker arrival required, so the gate can
   * never wedge. Set to MAX_SAFE_INTEGER for the whole catch-up replay
   * (suppress everything, batch rows included), reset to the live edge
   * (the final maxRendered) when catch-up ends. */
  private notifyFloor = 0;

  constructor(
    conversationId: string,
    store: ConversationStoreLike,
    transport: Transport,
    tagmaId: string,
    label: string | null,
  ) {
    // Relay store key == the derived conversation id == the cache key.
    super(
      conversationId,
      store,
      transport,
      conversationId,
      transport.localSender,
    );
    this.tagmaId = tagmaId;
    this.label = label;
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

  /** Reroute replay rows into the in-flight before-page. The core's dedup
   * is one-directional (id <= maxRendered is dropped), so a scroll-up
   * page's rows would vanish; worse, a user_message whose text matches the
   * in-flight send would be mis-consumed as its echo, promoting the
   * optimistic line to a duplicate old id. Genuine echoes never take this
   * branch: a relay echo is stamped above the cursor, a direct-path echo
   * carries history_id 0, and acks/markers carry no content. */
  protected override applyReplyCore(
    reply: TagmaReply,
    sender: Participant | undefined,
  ): void {
    const page = this.beforePull;
    if (page && sender) {
      const id =
        reply.kind === "event" || reply.kind === "user_message"
          ? (reply.history_id ?? 0)
          : 0;
      if (id > 0 && id <= this.maxRendered) {
        page.rows.push({ sender, reply });
        return;
      }
    }
    super.applyReplyCore(reply, sender);
  }

  /** Unread counting at the unified line-entry point: the reducer
   *  path carries both live frames and catch-up/refresh rows, so offline-
   *  window replay lines are counted exactly once by the store's watermark
   *  fence. Only the peer's authored content counts: `event` frames are the
   *  tagma's lines; `user_message` replays are my own echo (never unread);
   *  errors/markers land with synthetic ids and are skipped by the id test. */
  protected override onLineLanded(reply: TagmaReply, realId: number): void {
    super.onLineLanded(reply, realId);
    if (reply.kind === "event" && realId > 0) {
      unreadStore.observeTagmaLine(this.tagmaId, realId);
    }
  }

  protected override onReply(reply: TagmaReply): void {
    if (reply.kind === "history_batch_end") {
      const pull = this.pendingPulls.get(reply.req_id);
      if (pull) {
        this.pendingPulls.delete(reply.req_id);
        if (this.beforePull === pull) this.beforePull = null;
        pull.settle?.(reply.count, reply.more);
      }
      // Close the window bracket: drain-side batches advance only
      // maxRendered, so the scroll sentinel needs minRendered recomputed
      // once the batch has landed (idempotent; before-pages recompute
      // again through the merge).
      this.recomputeMinRendered();
      return;
    }
    if (shouldNotify(this.notifyFloor, reply)) {
      maybeNotifyBackground(this.tagmaId, this.label, reply);
    }
  }

  /** Backfill from the hydrated high-water to the newest server row: pages
   * `history{after}` until the cursor stops advancing, a batch comes back
   * empty, or the server says no more. A fresh device (empty cache) takes
   * one recent batch instead of the loop. Rows flow row-by-row through
   * the drain (the normal path: dedup, cache write, echo promotion all
   * apply); only the loop control lives here. */
  async catchUp(k = WINDOW_PAGE): Promise<void> {
    // Suppress notifications for the whole catch-up replay: every batch
    // row is old news re-arriving, and a fresh device's recent batch
    // starts below no floor at all (maxRendered 0) — either way one
    // notification per row would be spam. The floor resets to the live
    // edge (the final maxRendered) when catch-up ends, so only frames
    // newer than everything replayed can ever notify.
    this.notifyFloor = Number.MAX_SAFE_INTEGER;
    try {
      if (this.maxRendered > 0) {
        for (;;) {
          const before = this.maxRendered;
          const { count, more } = await this.pullPage({
            after: before,
            limit: k,
          });
          if (count === 0 || !more || this.maxRendered <= before) break;
        }
      } else {
        const { count, timedOut } = await this.pullPage({ limit: k });
        // A short recent batch IS the whole server history; disarm the
        // sentinel so it never arms for a page that cannot exist. A
        // timed-out batch proves nothing — leave the sentinel armed.
        if (!timedOut && count < k) this.hasMoreOlder = false;
      }
    } catch {
      // Dead channel: the drain surfaces transport status; the window
      // stays on whatever the hydrate landed and live frames keep
      // arriving.
    } finally {
      // Marker or not, whatever the drain landed forms the window bracket
      // (covers a timed-out recent batch that streamed rows but no
      // marker).
      this.recomputeMinRendered();
      this.notifyFloor = this.maxRendered;
    }
  }

  /** The relay leaf's page source: cache-first (the shared per-tagma
   * cache — same key as the offline entry), then an encrypted before-page
   * below the cache floor. A timed-out page (lost marker) folds its
   * buffered rows and leaves the sentinel armed — the next scroll
   * retries. */
  protected override async loadOlderPage(k: number): Promise<void> {
    const head = this.minRendered;
    const cached = await readTailBefore(this.cacheConversationId, head, k);
    if (cached.length > 0) {
      this.mergeWindowLines(cached.map(cachedLineToLine));
    }
    if (cached.length >= k) return;
    const { rows, more, timedOut } = await this.pullPage({
      before: head,
      limit: k,
    });
    const added = this.applyPulledRows(rows);
    if (!timedOut && (!more || added === 0)) this.hasMoreOlder = false;
  }

  /** Issue one cursor page and await ITS batch_end marker
   * (req_id-correlated; the rows stream through the drain meanwhile). The
   * per-batch timeout is the old watchdog's replacement: on expiry the
   * buffered rows fold, the page reads as terminal, and retry policy
   * stays with the caller. */
  private async pullPage(opts: {
    after?: number | null;
    before?: number | null;
    limit: number;
  }): Promise<{
    rows: HistoryEntry[];
    count: number;
    more: boolean;
    timedOut: boolean;
  }> {
    const pull: RelayPull = { rows: [], settle: null };
    const isBeforePage = (opts.before ?? 0) > 0;
    // The try covers the history() call too: if the channel rejects the
    // send, the finally still drops this pull off beforePull — an orphan
    // reroute target would otherwise swallow rows into a dead page.
    let req_id = -1;
    try {
      if (isBeforePage) this.beforePull = pull;
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
          });
        }, this.pullTimeoutMs);
        pull.settle = (count, more) => {
          clearTimeout(timer);
          resolve({ rows: pull.rows, count, more, timedOut: false });
        };
      });
    } finally {
      if (req_id >= 0) this.pendingPulls.delete(req_id);
      if (isBeforePage && this.beforePull === pull) {
        this.beforePull = null;
      }
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
    );
    this.status = "open";
  }

  /** The direct transport when still attached (null once the drain died). */
  private get direct(): DirectTransport | null {
    return this.transport instanceof DirectTransport ? this.transport : null;
  }

  /** Stream lifecycle from the direct transport's retry loop (wired by
   * attachLocal): "reconnecting" flips the status — the chat page shows its
   * in-chat spinner and the composer disables via status !== "open";
   * "resumed" restores "open" and backfills whatever frames the reconnect
   * gap dropped (cursor-based catch-up is idempotent and live-safe). */
  onTransportState(s: TransportState): void {
    if (!this.isLive()) return;
    if (s === "reconnecting") {
      this.status = "reconnecting";
    } else {
      this.status = "open";
      // Back online: flush the durable unsent lines once (M2: retrySend is
      // idempotent, so racing retries collapse).
      void this.retryAllPending();
      void this.catchUp();
    }
  }

  /** Backfill from the high-water mark to the newest server row: pages
   *  pullHistory({after: maxRendered}) until the batch stops advancing.
   *  On an empty window (a fresh device with no cache) one recent batch
   *  replaces the gap loop. Live-safe: applyPulledRows merges regardless
   *  of the order live frames land in. Fire-and-forget from attachLocal —
   *  the SSE drain runs concurrently and the UI is interactive meanwhile. */
  async catchUp(k = WINDOW_PAGE): Promise<void> {
    try {
      const t = this.direct;
      if (!t) return;
      if (this.maxRendered > 0) {
        for (;;) {
          const { rows, more } = await t.pullHistory({
            after: this.maxRendered,
            limit: k,
          });
          if (rows.length === 0) break;
          const before = this.maxRendered;
          this.applyPulledRows(rows);
          if (this.maxRendered <= before || !more) break;
        }
      } else {
        const { rows } = await t.pullHistory({ limit: k });
        this.applyPulledRows(rows);
        // A short recent batch IS the whole server history (recent-N
        // carries no `more`); disarm the sentinel so it never arms
        // again for a page that cannot exist.
        if (rows.length < k) this.hasMoreOlder = false;
      }
    } catch {
      // Server unreachable: the drain surfaces transport status; the window
      // stays on whatever the cache hydrated and live frames keep arriving.
    }
  }
  /** The local leaf's page source. Cache-first: a full cache page never
   *  touches the tagma (the offline degrade path — the cache holds everything
   *  ever rendered); a short cache page falls through to the server for the
   *  remainder. A zero-add server page disarms the sentinel (the oldest
   *  reachable row is already in the window). Guards, single-flight, and
   *  quiet error handling live in the base loadOlder. */
  protected override async loadOlderPage(k: number): Promise<void> {
    const head = this.minRendered;
    const cached = await readTailBefore(this.cacheConversationId, head, k);
    if (cached.length > 0) {
      this.mergeWindowLines(cached.map(cachedLineToLine));
    }
    if (cached.length >= k) return;
    const t = this.direct;
    if (!t) return;
    const { rows, more } = await t.pullHistory({ before: head, limit: k });
    const added = this.applyPulledRows(rows);
    if (!more || added === 0) this.hasMoreOlder = false;
  }
}

/** The degraded offline view of a relay conversation: the key exchange
 * could not run (the tagma is unreachable), so there is no transport --
 * the transcript hydrates from the per-device cache, sends land in the
 * pending store for a later retry/auto-flush, and scroll-up pages come
 * from the cache alone. Registered under the mapped conversation id AND
 * carrying the tagma id, so a successful re-open tears this down (via
 * findByTagma) instead of coexisting with the live conversation. */
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

  /** The base send early-returns without a transport; the offline view
   *  sends by design -- the line renders (failed, retryable) and lands in
   *  the pending store for the reconnect auto-flush. No pump: there is
   *  nothing to pump into until a real channel opens. */
  override send(text: string, attachment?: FileAttachment): void {
    const trimmed = text.trim();
    if (trimmed === "" && attachment === undefined) return;
    const localId = this.renderPendingLine(trimmed, attachment);
    this.transcript = sendFailed(this.transcript, localId, chat_send_failed());
  }

  /** Cache-first page source: offline there is nothing else. */
  protected override async loadOlderPage(k: number): Promise<void> {
    const head = this.minRendered;
    if (!head) return;
    const cached = await readTailBefore(this.cacheConversationId, head, k);
    this.mergeWindowLines(cached.map(cachedLineToLine));
    if (cached.length < k) this.hasMoreOlder = false;
  }
}
