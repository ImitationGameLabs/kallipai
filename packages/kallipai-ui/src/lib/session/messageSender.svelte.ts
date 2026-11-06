// The write path's carrier, deliberately not the read-path transport. A
// conversation's sends must survive the stream dying and being rebuilt: the
// queue, the POST and its settlement are the sender's business, while the SSE
// view ({@link Transport}) may be replaced under it at any time.
//
// The single-in-flight pump ({@link ConversationBase.pumpPending}) drives both
// transports through this seam, so the relay keeps one pump rather than a
// fork of its own.
import type { TagmaClient } from "@kallipai/kallipai-client";
import type { AgentId } from "@kallipai/kallipai-common";
import type { HistoryEntry } from "@kallipai/kallipai-lesche-client";
import type { RelayChannel } from "@kallipai/kallipai-keryx-client";

import type { SendOptions } from "./transport.ts";

/** What one POST resolved to. */
export interface SendResult {
  /** The relay channel's request id, when the transport mints one — the
   *  relay's `error` replies correlate on it. The direct path has none. */
  readonly reqId?: number;
  /** The accepted row's history id. null = the response carried no id: the
   *  settlement then rides the event frame (relay) or the echo plus watchdog
   *  (the direct degraded write path, whose frame is published with id 0). */
  readonly historyId: number | null;
}

/** One cursor-driven history page. `undecodable` counts rows the window held
 *  that the decoder could not read: non-zero means this page is a hole rather
 *  than a complete window. */
export interface HistoryPage {
  readonly rows: HistoryEntry[];
  readonly more: boolean;
  readonly undecodable: number;
}

export interface PullOpts {
  readonly after?: number | null;
  readonly before?: number | null;
  readonly limit?: number;
}

export interface MessageSender {
  /** Whether the carrier is still alive: `available = !closed`, so it says
   *  "this sender was not closed", not "a send can go through now". Both
   *  implementations keep `closed` in `$state`, so a reader that derives from
   *  this flag re-evaluates when the sender closes (the write axis does).
   *
   *  The queue is pumped by these explicit triggers, all in
   *  conversation.svelte.ts: send() on a new line, pumpPending() after a
   *  response settles a line, the reply echo (applyReplyCore), the send
   *  watchdog, and onTransportState -> retryAllPending when the stream
   *  resumes. A queued line meets this flag as a gate at each pump entry; the
   *  flag going false does not itself pump anything, it only closes the door. */
  readonly available: boolean;
  send(text: string, options?: SendOptions): Promise<SendResult>;
  /** Present where the read path cannot replay on connect (the direct path);
   *  the relay pages through its own store instead. */
  pullHistory?(opts: PullOpts): Promise<HistoryPage>;
  close(): void;
}

/** The direct (non-relay) write path: a plain inbound POST to the tagma, plus
 *  the cursor-driven history pull that backfills the window after a gap. */
export class DirectMessageSender implements MessageSender {
  private closed = $state(false);

  constructor(
    private readonly client: TagmaClient,
    private readonly agentId: AgentId,
  ) {}

  get available(): boolean {
    return !this.closed;
  }

  async send(text: string, options?: SendOptions): Promise<SendResult> {
    const resp = await this.client.postMessage(
      this.agentId,
      text,
      options?.attachment,
      options?.quote,
    );
    // The tagma names the row it appended, so the line can settle without the
    // echoed frame; a response without one (older tagma, degraded write) leaves
    // the echo to finish the job.
    return { historyId: resp.history_id ?? null };
  }

  async pullHistory(opts: PullOpts): Promise<HistoryPage> {
    const resp = await this.client.externalHistory(this.agentId, opts);
    return {
      rows: resp.rows as HistoryEntry[],
      more: resp.more,
      undecodable: resp.undecodable ?? 0,
    };
  }

  close(): void {
    this.closed = true;
  }
}

/** The relay write path: the E2EE channel's op, whose `req_id` correlates the
 *  eventual `error` reply. The accepted row's id arrives on the
 *  `message_accepted` frame, not the 202 that `send` resolves on. */
export class RelayMessageSender implements MessageSender {
  private closed = $state(false);

  constructor(private readonly channel: RelayChannel) {}

  get available(): boolean {
    return !this.closed;
  }

  async send(text: string, options?: SendOptions): Promise<SendResult> {
    const reqId = await this.channel.send(
      text,
      options?.attachment,
      options?.quote,
    );
    return { reqId, historyId: null };
  }

  close(): void {
    this.closed = true;
  }
}
