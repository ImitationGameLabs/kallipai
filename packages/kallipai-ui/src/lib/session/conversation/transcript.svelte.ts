// The read model of one conversation: the rendered rows, the positions they
// carry, the row-id bracket the pager pages by, and the numbering freeze.
//
// The fields are one shared cell that the callers land a single step into:
// frames.ts stamps a reply, outbox.svelte.ts settles or fails a line, the
// leaves move the bracket when a pulled or cached row arrives, and the base
// writes the window-level ones. Five files write here; the methods own the
// window-wide work -- merging a page, recomputing the bracket, advancing the
// cursor -- plus the two whole-window entries: restore() (rows and edges) and
// restoreResume() (rows, edges, both cursors). A write that is more than one
// step belongs in a method here.
//
// Ordering assumptions that hold here:
// - positions advance only over a contiguous run (seq === lastSeq + 1);
// - the floor only moves down, maxRowId only up, and recomputeMin is
//   idempotent;
// - a frozen window (historyError === "protocol") renders what arrives and
//   places nothing: no position stamp, no cursor move, no cache row.

import type { Participant, TagmaReply } from "@kallipai/kallipai-lesche-client";
import { putCacheLine } from "./shared.ts";
import {
  cacheLineOf,
  EMPTY_TRANSCRIPT,
  historyEntryLine,
  mergeHistoryLines,
  stampPosition,
} from "../../transcript.ts";
import type {
  ConversationLine,
  ConversationTranscript,
} from "../../transcript.ts";
import type { HistoryEntry } from "@kallipai/kallipai-lesche-client";
import { contentSeq } from "./shared.ts";
import { HistorySchemaMismatch } from "../messageSender.svelte.ts";

export class Transcript {
  state: ConversationTranscript = $state(EMPTY_TRANSCRIPT);

  constructor(
    readonly cacheConversationId: string,
    private readonly onAdvance: (cursor: number) => void,
  ) {}

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
  /** The oldest durable id in the loaded window (the scroll-up cursor).
   *  `0` = nothing loaded. Together with `maxRowId` it brackets what
   *  the transcript currently holds; rows outside the bracket are fetched
   *  on demand (catch-up above, scroll-paging below), never up front.
   *  Second duty, dedup: a content frame below this floor was rendered and
   *  then let go, so it is a replay; anything at or above the floor is still
   *  eligible to land. */
  minRowId = $state(0);
  /** Set when the history could not be brought up -- a pull that failed, a
   *  cache that could not open, or a page written under a numbering this
   *  build does not read: that last one is `"protocol"`, which freezes the
   *  window (see {@link frozen}). The page shows it with a retry, or
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
  get frozen(): boolean {
    return this.historyError === "protocol";
  }

  /** Record a failed history pull: an answer written under another numbering
   *  freezes the window, anything else is the retryable case. A freeze is
   *  never downgraded -- the mismatch stands until a reload, whatever fails
   *  next. */
  notePullFailure(e: unknown): void {
    if (this.frozen) return;
    this.historyError =
      e instanceof HistorySchemaMismatch ? "protocol" : "unavailable";
  }

  /** Adopt a cached window with no stream to resume from: the rows, and the
   *  row-id edges that follow them. Both cursors stay at zero -- the offline
   *  view has no stream, so there is no position to continue from. */
  restore(lines: readonly ConversationLine[]): void {
    this.state = { ...EMPTY_TRANSCRIPT, lines: [...lines] };
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
  }

  get rows(): ConversationLine[] {
    return this.state.lines;
  }

  get status(): ConversationTranscript["status"] {
    return this.state.status;
  }

  get error(): ConversationTranscript["error"] {
    return this.state.error;
  }

  /** Fold one page of already-renderable lines into the window in a single
   *  transcript rebuild (the batch — not the row — is the update unit, so a
   *  50-row page costs one O(window) pass, not 50). Pure w.r.t. the
   *  transcript; the window edges and the scroll-up position recompute
   *  from the merged lines.
   *  Returns how many rows were newly added (0 = the page was fully
   *  redundant — the idempotency signal loadOlder uses to stop paging). */
  merge(rows: ConversationLine[]): number {
    const { transcript, added } = mergeHistoryLines(this.state, rows);
    // A page can name the position of a row that is already on screen
    // without adding a line: that fill is still a change, so it is applied
    // before the idempotency answer the caller pages on.
    if (added === 0 && transcript === this.state) return 0;
    this.state = transcript;
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
   *  live drain it never goes through applyReplyCore, so the cache write it
   *  owes the next hydrate is this method's own. */
  applyPulled(rows: HistoryEntry[]): number {
    const lines = rows
      .map(historyEntryLine)
      .filter((l): l is ConversationLine => l !== null);
    // A page already in flight when the freeze landed still renders below:
    // it hands over neither a cache row nor a position.
    if (!this.frozen) {
      for (const l of lines) putCacheLine(this.cacheConversationId, l);
      this.advance(lines.map((l) => l.seq ?? 0));
    }
    return this.merge(lines);
  }
  /** Recompute the window floor (`minRowId`) and the scroll-up position
   *  (`minSeq`) from the rendered lines. The drain-side
   *  batch path appends rows through applyReplyCore (which advances only
   *  `maxRowId`); this closes the bracket once a batch has landed so the
   *  scroll sentinel can arm. The floor only moves down and the scroll-up
   *  position only moves up, so a window that lost its oldest rendered row
   *  keeps a floor it already proved. Idempotent, O(window). */
  recomputeMin(): void {
    let min = Infinity;
    let minSeq = Infinity;
    for (const l of this.state.lines) {
      if (l.historyId > 0 && l.historyId < min) min = l.historyId;
      if (l.seq != null && l.seq > 0 && l.seq < minSeq) minSeq = l.seq;
    }
    if (min !== Infinity) this.minRowId = min;
    if (minSeq !== Infinity) this.minSeq = minSeq;
  }

  /** Adopt a cache hydrate on a live leaf. It lands three things: the rows,
   *  the row-id edges those rows imply, and the positions they already have
   *  -- the newest becomes the resume cursor and the oldest the scroll-up
   *  anchor. A hydrate is the one place a whole window arrives at once;
   *  every other landing comes through applyReplyCore or applyPulled. The
   *  rows land even in a frozen window, because they are what the reader has;
   *  a freeze withholds positions, not content. */
  restoreResume(lines: readonly ConversationLine[]): void {
    // Rows first, and unconditionally: a hydrate that dropped them would
    // leave the reader a cursor with nothing under it.
    this.state = { ...EMPTY_TRANSCRIPT, lines: [...lines] };
    if (this.frozen) return;
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
    this.recomputeMin();
    this.advance(lines.map((l) => l.seq ?? 0));
  }

  /** Move the resume cursor over positions this client just rendered. Only a
   *  position that continues the run at `lastSeq + 1` advances it, so a row
   *  landing above a hole never carries the cursor over that hole. A cursor
   *  with nothing behind it (0) takes the lowest rendered position and runs on
   *  from there: that window is the device's own start. */
  advance(seqs: readonly number[]): void {
    const above = seqs.filter((s) => s > this.lastSeq).sort((a, b) => a - b);
    let cursor = this.lastSeq;
    for (const s of above) {
      if (cursor === 0 || s === cursor + 1) cursor = s;
      else break;
    }
    if (cursor === this.lastSeq) return;
    this.lastSeq = cursor;
    this.onAdvance(cursor);
  }

  /** Adopt the position a frame reports for a row the transcript already
   *  renders: an ack-promoted line, or a replay of a settled one. The line
   *  takes the position, its cache copy follows, and a position that continues
   *  the run moves the cursor. A jump does not resume: the row is on screen,
   *  so no hole sits under it. */
  adopt(
    reply: TagmaReply,
    historyId: number,
    sender: Participant | undefined,
  ): void {
    const seq = contentSeq(reply);
    if (historyId <= 0) return;
    // Frozen: the row renders, but its position comes from a numbering
    // this build cannot place (see `historyFrozen`).
    if (this.frozen) return;
    if (seq !== null) {
      this.state = stampPosition(this.state, historyId, seq);
      if (seq === this.lastSeq + 1) this.advance([seq]);
    }
    const cached = cacheLineOf(reply, sender);
    if (cached === null) return;
    putCacheLine(this.cacheConversationId, cached);
  }
}
