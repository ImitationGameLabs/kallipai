// The degraded offline view of a relay conversation: the key exchange could
// not run (the tagma is unreachable), so there is no transport, the transcript
// hydrates from the per-device cache, sends land in the pending store for a
// later retry or auto-flush, and scroll-up pages come from the cache alone.

import { sendFailed } from "../../transcript.ts";
import { cachedLineToLine } from "./shared.ts";
import type { ConversationSender } from "../../transcript.ts";
import type { HistoryEntry } from "@kallipai/kallipai-lesche-client";
import { WriteRefusal } from "../writeRefusal.ts";
import { readTailBefore } from "@kallipai/kallipai-keryx-client";
import type { SendOptions } from "../transport.ts";
import { chat_send_failed } from "../../../paraglide/messages.js";
import { ConversationBase, type ConversationStoreLike } from "./base.svelte.ts";

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
    this.link.status = "offline";
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
    if (!this.link.canWrite) {
      this.link.error = new WriteRefusal("closed");
      return;
    }
    const trimmed = text.trim();
    if (trimmed === "" && options?.attachment === undefined) return;
    const localId = this.outbox.renderPendingLine(trimmed, options);
    this.transcript.state = sendFailed(
      this.transcript.state,
      localId,
      chat_send_failed(),
    );
  }

  /** No retry on the offline view: with no transport the pump would
   *  throw inside its send and the catch would demote the durable
   *  queued row to failed, cancelling the reconnect auto-flush that
   *  send() deliberately preserves. The failed line keeps its retry
   *  button only where a live channel exists. */
  override retrySend(_localSeq: number): void {}
  /** Cache-first page source: offline there is nothing else. */
  protected override async loadOlderPage(k: number): Promise<void> {
    const head = this.transcript.minRowId;
    if (!head) return;
    const cached = await readTailBefore(this.cacheConversationId, head, k);
    this.transcript.merge(cached.map(cachedLineToLine));
    // The cache is the only source offline: a short page means it holds
    // nothing older, and no transport could have dropped rows.
    if (cached.length < k) this.pager.hasMoreOlder = false;
  }
}
