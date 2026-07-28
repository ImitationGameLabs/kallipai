// The offline (local, non-relay) transport: serves the tagma's external
// chat-room API over a plain HTTP+SSE connection with no relay process and no
// E2EE. It is the frontend's sole window onto a directly-connected tagma --
// authored assistant messages, runtime signals (busy/idle presence, turn
// terminals, errors), and status snapshots all arrive on the one multiplexed
// SSE, demuxed here by the `event:` field name.
//
// The Transport contract exposes three streams (`replies` + `signals` +
// `status`) but the direct wire is one SSE, so an internal drain fans each
// decoded frame out into three queues. The direct path has no
// `message_accepted` ack: the user's inbound POST resolves synchronously with
// no `history_id`, so the store renders the optimistic user line as sent once
// the POST resolves.

import type { TagmaClient } from "@kallipai/kallip-client";
import type { SignalEvent, TagmaReply } from "@kallipai/kallip-lesche-client";
import type { TagmaStatusSummary } from "../tagmata.svelte.ts";
import { AsyncQueue, type Transport } from "./transport.ts";

/** The aggregate runtime snapshot on the direct stream (the snake_case wire
 * shape, mirroring the relay's `tagma_status` LescheEvent). Ephemeral operator
 * metadata; the status header renders it. The transcript is driven by signals,
 * not by these snapshots. */
export interface DirectStatusPayload {
  readonly root_state: "idle" | "busy" | "faulted";
  readonly subagents_total: number;
  readonly subagents_active: number;
  readonly token_budget: number;
  readonly token_consumed: number;
}

/** One frame on the direct external SSE, discriminated by the SSE `event:`
 * field. Authored content is persisted/replayable on the tagma; signals and
 * status are ephemeral. */
export type DirectFrame =
  | { readonly kind: "authored"; readonly reply: TagmaReply }
  | { readonly kind: "signal"; readonly event: SignalEvent }
  | { readonly kind: "status"; readonly payload: DirectStatusPayload };

/** Map the snake_case direct status payload to the transport-neutral
 *  `TagmaStatusSummary` (the same shape the relay path's agora-SSE status maps
 *  to), so the conversation drains one uniform type from either transport. */
function toSummary(p: DirectStatusPayload): TagmaStatusSummary {
  return {
    rootState: p.root_state,
    subagentsTotal: p.subagents_total,
    subagentsActive: p.subagents_active,
    tokenBudget: p.token_budget,
    tokenConsumed: p.token_consumed,
  };
}

/**
 * Wraps a {@link TagmaClient} bound to the root agent and exposes the external
 * chat-room API: iterate {@link replies} / {@link signals} / {@link status}
 * (the Transport view) or {@link frames} (the raw demuxed union), and
 * {@link send} a user message. {@link close} aborts the SSE stream.
 *
 * NOTE: until the offline cutover completes, `frames()` and `replies()` /
 * `signals()` / `status()` are mutually exclusive on one instance -- each opens
 * its own SSE. The store uses `frames()`; the mux (which feeds the Transport
 * generators) is lazy and dormant until a caller iterates them. Do not mix on
 * the same transport.
 */
export class DirectTransport implements Transport {
  private readonly controller = new AbortController();
  private readonly replyQueue = new AsyncQueue<TagmaReply>();
  private readonly signalQueue = new AsyncQueue<SignalEvent>();
  private readonly statusQueue = new AsyncQueue<TagmaStatusSummary>();
  private muxStarted = false;

  constructor(
    private readonly client: TagmaClient,
    readonly agentId: string,
  ) {}

  async *replies(): AsyncGenerator<TagmaReply> {
    this.ensureMux();
    while (true) {
      const { value, done } = await this.replyQueue.next();
      if (done) return;
      yield value;
    }
  }

  async *signals(): AsyncGenerator<SignalEvent> {
    this.ensureMux();
    while (true) {
      const { value, done } = await this.signalQueue.next();
      if (done) return;
      yield value;
    }
  }

  async *status(): AsyncGenerator<TagmaStatusSummary> {
    this.ensureMux();
    while (true) {
      const { value, done } = await this.statusQueue.next();
      if (done) return;
      yield value;
    }
  }

  async send(text: string): Promise<void> {
    await this.client.postMessage(this.agentId, text);
  }

  /** Pull a cursor-driven history batch and feed it through the SAME `replyQueue`
   *  live `authored` frames use (followed by a synthesized `history_batch_end`),
   *  so the conversation's reply drain consumes live + batch frames in one
   *  ordered stream — mirroring how `RelayChannel.history` routes its batch back
   *  through `replies()`. Idempotent to call before `replies()` is iterated: the
   *  mux drains the queue regardless of iteration order. */
  async history(opts: {
    after?: number | null;
    before?: number | null;
    limit?: number;
  }): Promise<void> {
    const resp = await this.client.externalHistory(this.agentId, opts);
    for (const row of resp.rows) this.replyQueue.push(row);
    // Synthesize the batch-end marker so the reducer's `history_batch_end` arm
    // (and any future `live` gate reuse) sees the same terminator the relay
    // path emits. The direct path does not use the `live` gate today; the
    // marker is consumed for parity + forward-compat.
    this.replyQueue.push({
      kind: "history_batch_end",
      req_id: 0,
      count: resp.rows.length,
      more: resp.more,
    });
  }

  close(): void {
    this.controller.abort();
    this.replyQueue.close();
    this.signalQueue.close();
    this.statusQueue.close();
  }

  // --- internal: the single SSE demux, fanned out into the three queues ---

  /** The reply/signal/status generators each lazily start the demux the first
   *  time one is iterated; whichever runs first owns the single drain. */
  private ensureMux(): void {
    if (this.muxStarted) return;
    this.muxStarted = true;
    void this.runMux();
  }

  private async runMux(): Promise<void> {
    try {
      for await (const f of this.frames()) {
        switch (f.kind) {
          case "authored":
            this.replyQueue.push(f.reply);
            break;
          case "signal":
            this.signalQueue.push(f.event);
            break;
          case "status":
            this.statusQueue.push(toSummary(f.payload));
            break;
        }
      }
    } catch (e) {
      // Propagate the failure to every drain so the store's run() surfaces it.
      this.replyQueue.fail(e);
      this.signalQueue.fail(e);
      this.statusQueue.fail(e);
      return;
    }
    this.replyQueue.close();
    this.signalQueue.close();
    this.statusQueue.close();
  }

  /** Iterate the multiplexed external SSE, yielding decoded frames until the
   *  stream ends or {@link close} aborts it. A malformed payload is dropped
   *  rather than killing the stream (one bad frame must not lose the
   *  connection). */
  async *frames(): AsyncGenerator<DirectFrame> {
    for await (const f of this.client.externalEventStream(
      this.agentId,
      this.controller.signal,
    )) {
      try {
        switch (f.event) {
          case "authored":
            yield { kind: "authored", reply: JSON.parse(f.data) as TagmaReply };
            break;
          case "signal":
            yield { kind: "signal", event: JSON.parse(f.data) as SignalEvent };
            break;
          case "status":
            yield {
              kind: "status",
              payload: JSON.parse(f.data) as DirectStatusPayload,
            };
            break;
          default:
            // Unknown event name: ignore (forward-compat with new frame kinds).
            break;
        }
      } catch {
        // Drop undecodable payloads; the next frame must still arrive.
      }
    }
  }
}
