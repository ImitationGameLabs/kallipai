// The send side of one conversation: the optimistic line and the durable
// pending row behind it, the single-in-flight POST pump, the reply watchdog,
// and the discard ledger.
//
// These rules sit together here: one ack in flight at a time, a queued row
// re-sent at least once, a failed line waiting for the user instead of
// retrying itself, and a lost reply failing its line rather than stalling the
// pump.

import type { FileAttachment } from "@kallipai/kallipai-lesche-client";
import {
  deletePending,
  isFailedPending,
  markPendingFailed,
  type PendingLine,
  putPending,
  readPendingByTagma,
} from "@kallipai/kallipai-keryx-client";
import { sendFailureCopy } from "../../errors.ts";
import {
  dropFailedLine,
  nextPendingSeq,
  replaceLineId,
  retryLine,
  sendFailed,
  withUserLine,
} from "../../transcript.ts";
import type { ConversationSender } from "../../transcript.ts";
import { putCacheLine } from "./shared.ts";
import type { SendOptions } from "../transport.ts";
import type { MessageSender } from "../messageSender.svelte.ts";
import type { Transcript } from "./transcript.svelte.ts";
import { chat_send_timed_out } from "../../../paraglide/messages.js";

/** One queued optimistic line: the rendered local id, its text and extras. */
export interface PendingEntry {
  localId: number;
  text: string;
  attachment?: FileAttachment;
  quote?: number;
}

/** The one POST in flight: its local id, the text the echo must match, and
 *  the req_id the channel stamps on accept. */
export interface InFlightSend {
  localId: number;
  text: string;
  reqId?: number;
}

/** How long an accepted send waits for its ack/error reply before the
 *  watchdog presumes the reply lost. Aligned with the SSE connect timeout
 *  and the per-attempt fetch cap (30s each). */
const SEND_WATCHDOG_MS = 30_000;

export class Outbox {
  /** Queued optimistic lines: each already rendered in the read model with
   *  status "sending"; the pump takes them one ack at a time. */
  pending = $state<PendingEntry[]>([]);

  /** Discarded local ids. Rule: any read that redraws lines from the store
   *  consults this first -- a row can be in a snapshot taken before the delete
   *  landed, and a discard must not be undone by a reconnect. */
  readonly discarded = new Set<number>();

  /** The ONE in-flight POST (its `user_message` frame has not landed): its
   *  synthetic id + sent text, or null when the pump is idle. The text lets the
   *  promotion branch correlate the echo to this exact send, so a history-replay
   *  `user_message` arriving mid-flight is not mistaken for the ack. */
  pendingInFlight = $state<InFlightSend | null>(null);

  /** The most recent pending stamp this session allocated (the monotonic
   *  floor nextPendingSeq decrements below; plain, not observed). */
  private lastPendingSeq = 0;

  /** Watchdog for the in-flight POST: if neither the ack nor an error
   *  reply consumes `pendingInFlight` within the window, the pump would
   *  stall forever (a lost reply rides no transport failure). The timer
   *  fails the line (retryable) and releases the slot. */
  private pumpWatchdog: ReturnType<typeof setTimeout> | null = null;

  constructor(
    /** The IndexedDB cache key a confirmed line is written under. */
    private readonly cacheConversationId: string,
    /** The read model the lines are rendered into and confirmed in. */
    private readonly transcript: Transcript,
    /** The UI sender an optimistic line is attributed to. */
    private readonly localSender: ConversationSender,
    /** The pending-store partition this conversation's unsent lines live
     *  under (the tagma id; the offline shell's local conversation uses the
     *  literal "local", mirroring its cache-key fallback). Read per call: a
     *  leaf may answer it from a field. */
    private readonly pendingKey: () => string,
    /** The write path's carrier, read per pump: null or closed leaves the
     *  queue alone (the pump is the queue's writer, not its owner). */
    private readonly sender: () => MessageSender | null,
  ) {}

  /** Render + persist one locally authored line, then hand it to the pump.
   *  The caller trims and empty-gates the text. */
  enqueue(trimmed: string, options?: SendOptions): void {
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
  renderPendingLine(trimmed: string, options?: SendOptions): number {
    const attachment = options?.attachment;
    const quote = options?.quote;
    const now = new Date();
    const localId = nextPendingSeq(now.getTime(), this.lastPendingSeq);
    this.lastPendingSeq = localId;
    this.transcript.state = withUserLine(
      this.transcript.state,
      trimmed,
      localId,
      this.localSender,
      attachment,
      now,
      quote,
    );
    void putPending({
      tagmaId: this.pendingKey(),
      localSeq: localId,
      text: trimmed,
      ...(attachment !== undefined ? { attachment } : {}),
      ...(quote !== undefined ? { quote } : {}),
      createdAt: now.toISOString(),
      status: "queued",
    });
    return localId;
  }

  /** Re-send one failed line (the bubble's retry button). Idempotent: a
   *  line that is not failed, already in flight, or already queued is a
   *  no-op. */
  retrySend(localSeq: number): void {
    const line = this.transcript.state.lines.find(
      (l) => l.historyId === localSeq,
    );
    if (!line || line.status !== "failed") return;
    if (this.pendingInFlight?.localId === localSeq) return;
    if (this.pending.some((p) => p.localId === localSeq)) return;
    this.transcript.state = retryLine(this.transcript.state, localSeq);
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
    const line = this.transcript.state.lines.find(
      (l) => l.historyId === localSeq,
    );
    if (!line || line.status !== "failed") return false;
    if (this.pendingInFlight?.localId === localSeq) return false;
    this.transcript.state = dropFailedLine(this.transcript.state, localSeq);
    this.pending = this.pending.filter((p) => p.localId !== localSeq);
    this.discarded.add(localSeq);
    void deletePending(this.pendingKey(), localSeq);
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
      rows = await readPendingByTagma(this.pendingKey());
    } catch {
      return; // IndexedDB unavailable (private mode): nothing to flush
    }
    for (const row of rows) {
      if (this.discarded.has(row.localSeq)) {
        // Discarded after this read began: the line is gone from the transcript
        // and must not come back. Re-issuing the delete heals a miss.
        void deletePending(this.pendingKey(), row.localSeq);
        continue;
      }
      const failed = isFailedPending(row);
      // Render (or re-render) in stored order, whatever the branch: the
      // bubble sequence must follow the rows' original timing.
      if (
        !this.transcript.state.lines.some((l) => l.historyId === row.localSeq)
      ) {
        let t = withUserLine(
          this.transcript.state,
          row.text,
          row.localSeq,
          this.localSender,
          row.attachment,
          new Date(row.createdAt),
          row.quote,
        );
        if (failed) t = sendFailed(t, row.localSeq, row.lastError);
        this.transcript.state = t;
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

  /** Single-in-flight send pump. POSTs the next queued optimistic line (if
   *  any, and if no POST is already outstanding), leaving its localId + text
   *  in `pendingInFlight` so the stamped `user_message` frame can correlate.
   *  On a POST failure the line is kept and marked failed via `sendFailed`
   *  (retryable), with the failure copy inline. */
  async pumpPending(): Promise<void> {
    if (this.pendingInFlight !== null) return;
    const sender = this.sender();
    // A missing or closed sender leaves the line queued.
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
      // Our own sentence rides the line; the server's message and the code
      // stay with this log and sendFailureCopy, so a bilingual interface
      // never shows a single-locale sentence by accident.
      console.error("[chat] send failed:", e);
      const failure = sendFailureCopy(e);
      // Local send failure: red status error only -- no system history
      // line. A genuine server kind:"error" reply (the wire path)
      // still enters history via the reducer; that is by-design and
      // stays. Feeding this failure through the same reducer is what
      // double-rendered it (system line + red banner).
      this.failInFlight(next.localId, failure.copy, failure.code);
    }
  }

  /** Fail one in-flight line with the given copy: the line turns retryable,
   *  its durable row records the failure, and the pump slot frees so the next
   *  queued line can go. Used by a POST failure, a lost-reply watchdog, and a
   *  correlated op error. */
  failInFlight(localId: number, copy: string, code?: string): void {
    this.transcript.state = sendFailed(
      this.transcript.state,
      localId,
      copy,
      code,
    );
    void markPendingFailed(this.pendingKey(), localId, copy);
    this.clearWatchdog();
    this.pendingInFlight = null;
    void this.pumpPending();
  }

  /** Release the pump slot after the line settled through the read path (the
   *  wire ack closed it): nothing to fail, just stop waiting and advance. */
  releaseInFlight(): void {
    this.pendingInFlight = null;
    this.clearWatchdog();
    void this.pumpPending();
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
    this.transcript.state = replaceLineId(
      this.transcript.state,
      localId,
      ackId,
      undefined,
      wireSender,
    );
    const confirmed = this.transcript.state.lines.find(
      (l) => l.historyId === ackId,
    );
    if (confirmed && !this.transcript.frozen) {
      // A frozen window writes no cache rows: the id this row just took
      // belongs to a numbering this build does not read, and a cached row
      // is a position the next hydrate would trust.
      // Plain `wireSender`, never the $state proxy (structured clone rejects it).
      putCacheLine(this.cacheConversationId, {
        historyId: ackId,
        role: "user",
        text: confirmed.text,
        sender: wireSender,
        createdAt: confirmed.createdAt,
        attachment: confirmed.attachment,
        quote: confirmed.quote,
      });
    }
    void deletePending(this.pendingKey(), localId);
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
      this.failInFlight(localId, copy);
      console.warn("[chat] send reply lost; failed the line after 30s");
    }, SEND_WATCHDOG_MS);
  }

  clearWatchdog(): void {
    if (this.pumpWatchdog !== null) {
      clearTimeout(this.pumpWatchdog);
      this.pumpWatchdog = null;
    }
  }
}
