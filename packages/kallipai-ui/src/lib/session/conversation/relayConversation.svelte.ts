// The online leaf: the E2EE pipe. History pages are cursor pulls that await
// their batch_end marker; a catch-up ADOPTS the newest window as one batch
// (one fold) instead of replaying the gap row by row; scroll-up pages continue
// below the window head, and replayed rows below the rendered cursor reroute
// into the pending page. Only frames above the open-time high-water fire
// background notifications.

import { cachedLineToLine, contentId, WINDOW_PAGE } from "./shared.ts";
import type {
  HistoryEntry,
  Participant,
  TagmaReply,
} from "@kallipai/kallipai-lesche-client";
import type { CachedLine, RelayChannel } from "@kallipai/kallipai-keryx-client";
import { manageOk } from "@kallipai/kallipai-keryx-client";
import { relayRootAgentId } from "../rootAgent.ts";
import { readTailBefore } from "@kallipai/kallipai-keryx-client";
import type { Transport } from "../transport.ts";
import type { MessageSender } from "../messageSender.svelte.ts";
import { RelayMessageSender } from "../messageSender.svelte.ts";
import { notify } from "../notify.ts";
import { PUT_THROTTLE_MS, tagmaKey, unreadStore } from "../unread.svelte.ts";
import { ConversationBase, type ConversationStoreLike } from "./base.svelte.ts";

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
    return this.link.transport !== null && this.link.status === "open";
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
    if (this.link.sender === null) {
      this.link.sender = new RelayMessageSender(
        this.relayTransport.relayChannel,
      );
    }
  }

  /** The E2EE transport (for the store's history pull, envelope delivery,
   * and signal routing). */
  get relayTransport(): import("../relayTransport.ts").RelayTransport {
    return this.link.transport as import("../relayTransport.ts").RelayTransport;
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
    this.reportReadCursor(this.transcript.lastSeq);
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
    if (page && sender && id > 0 && id <= this.transcript.maxRowId) {
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
      this.outbox.pendingInFlight !== null &&
      reply.text.trim() === this.outbox.pendingInFlight.text.trim()
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
      this.transcript.recomputeMin();
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
      if (!page.timedOut && rawRows < k) this.pager.hasMoreOlder = false;
    } catch {
      // Dead channel: the drain surfaces transport status; the window stays on
      // whatever the hydrate landed and live frames keep arriving.
    } finally {
      // Marker or not, whatever landed forms the window bracket (covers a
      // timed-out window that streamed rows but no marker).
      this.transcript.recomputeMin();
      this.notifyFloor = this.transcript.maxRowId;
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
    this.transcript.applyPulled(rows);
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
    const durable = this.transcript.state.lines.filter((l) => l.historyId > 0);
    if (durable.length === 0) return;
    const max = Math.max(...durable.map((l) => l.historyId));
    if (max >= headId - 1) return;
    this.transcript.state = {
      ...this.transcript.state,
      lines: this.transcript.state.lines.filter((l) => l.historyId <= 0),
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
      if (cachedTop >= this.transcript.minRowId - 1) {
        this.transcript.merge(cached.map(cachedLineToLine));
        if (cached.length >= k) return;
      }
    }
    // Page below the window floor as it stands NOW: when a cached page was
    // merged above, this continues beneath it instead of re-fetching the rows
    // just rendered (a re-fetch folds to `added === 0` and would disarm the
    // sentinel with history still below).
    const { rows, more, timedOut } = await this.pullPage({
      before: this.transcript.minRowId,
      limit: k,
    });
    const added = this.transcript.applyPulled(rows);
    // added === 0 disarms for a different reason than !more: a page whose rows
    // all failed to decode cannot move the cursor, so re-paging returns it again.
    if (!timedOut && (!more || added === 0)) this.pager.hasMoreOlder = false;
  }

  /** The cache page below the window head. A seam so the adjacency rule is
   *  testable without IndexedDB (the production read is the shared per-tagma
   *  cache -- the same key the offline entry uses). */
  protected readCachedPage(k: number): Promise<CachedLine[]> {
    return readTailBefore(
      this.cacheConversationId,
      this.transcript.minRowId,
      k,
    );
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
    if (this.link.transport === null || this.link.closed) {
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
    if (
      this.link.transport === null ||
      this.link.closed ||
      this.windowPull !== null
    ) {
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
    this.outbox.pending = [];
    this.outbox.pendingInFlight = null;
    this.outbox.clearWatchdog();
    this.transcript.state = {
      ...this.transcript.state,
      lines: this.transcript.state.lines.filter((l) => l.status !== "sending"),
    };
  }
}
