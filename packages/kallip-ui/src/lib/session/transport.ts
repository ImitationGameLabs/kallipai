// The transport-agnostic seam every conversation drains. Both the direct
// (offline) and relayed (online) paths implement this; the per-conversation
// `Conversation` unit is the sole consumer, so it never knows which wire shape
// is underneath.
//
// The interface splits the stream into the three channels the reducer cares
// about -- authored replies, runtime signals, and status snapshots -- because
// the two transports source them differently and that asymmetry is best
// encapsulated inside each transport:
//   - DirectTransport demuxes all three off one SSE.
//   - RelayTransport pulls replies from the E2EE RelayChannel and signals from
//     a queue the realtime feed pushes into; status is EMPTY here (the relay's
//     status rides the agora SSE, routed into the conversation via the realtime
//     status sink, not this transport).
//
// `replies()` / `signals()` / `status()` THROW on a transport-level failure
// (the store's drain surfaces it as a connection error) and simply END on a
// clean close.

import type { SignalEvent, TagmaReply } from "@kallipai/kallip-lesche-client";
import type { TagmaStatusSummary } from "../tagmata.svelte.ts";

export interface Transport {
  /** The authored-content reply stream (acks, op errors, replayed user
   *  messages, authored `assistant_content`). Ends or throws when the transport
   *  closes or fails. */
  replies(): AsyncGenerator<TagmaReply>;
  /** The runtime-signal feed (busy/idle presence, turn terminals, errors). Same
   *  end/throw contract as {@link replies}. */
  signals(): AsyncGenerator<SignalEvent>;
  /** The aggregate status-snapshot feed (root state, subagent counts, token
   *  budget). Same end/throw contract as {@link replies}. Empty on the relay
   *  transport (status is realtime-driven there); live on the direct transport.
   *  The conversation drains this into its `statusSnapshot`, so the chat header
   *  has one uniform source regardless of transport. */
  status(): AsyncGenerator<TagmaStatusSummary>;
  /** Send a user message. Resolves when the wire accept lands (direct: 200;
   *  relay: 202). The tagma's reply (ack/error/authored) flows via
   *  {@link replies}. */
  send(text: string): Promise<void>;
  /** Tear down the underlying stream(s) synchronously. */
  close(): void;
}

/** A single-consumer async queue: one producer pushes (or fails), one consumer
 *  drains via `next()` inside an async generator. Items pushed before the
 *  consumer awaits are buffered; `close()` ends the drain cleanly; `fail(e)`
 *  rejects the consumer's pending `next()` (and any later one) so the draining
 *  generator throws. Used internally by the transports to demux/merge their two
 *  channels. */
export class AsyncQueue<T> {
  private buf: T[] = [];
  private pending: {
    resolve: (v: IteratorResult<T>) => void;
    reject: (e: unknown) => void;
  } | null = null;
  private closed = false;
  private failure: unknown = null;

  push(value: T): void {
    if (this.closed) return;
    if (this.pending) {
      const p = this.pending;
      this.pending = null;
      p.resolve({ value, done: false });
    } else {
      this.buf.push(value);
    }
  }

  /** End the drain cleanly. Buffered items are dropped (the consumer drains
   *  before seeing done). For a transport carrying persisted replies this is an
   *  acceptable loss: a close only happens on detach/mode-switch, and the next
   *  connect re-fetches via history replay (the store dedups by history_id). */
  close(): void {
    this.finish(null);
  }

  /** End the drain with an error: the consumer's pending or next `next()`
   *  rejects with `error`, so the draining async generator throws. */
  fail(error: unknown): void {
    this.finish(error);
  }

  private finish(error: unknown | null): void {
    if (this.closed) return;
    this.closed = true;
    this.failure = error;
    this.buf.length = 0;
    if (this.pending) {
      const p = this.pending;
      this.pending = null;
      if (error !== null) p.reject(error);
      else p.resolve({ value: undefined as unknown as T, done: true });
    }
  }

  next(): Promise<IteratorResult<T>> {
    // Single-consumer contract: two concurrent drains would orphan the first's
    // promise (it would never settle). Fail loud rather than hang.
    if (this.pending) {
      throw new Error(
        "AsyncQueue is single-consumer: a drain is already pending",
      );
    }
    if (this.buf.length) {
      return Promise.resolve({ value: this.buf.shift()!, done: false });
    }
    if (this.closed) {
      return this.failure !== null
        ? Promise.reject(this.failure)
        : Promise.resolve({ value: undefined as unknown as T, done: true });
    }
    return new Promise((resolve, reject) => {
      this.pending = { resolve, reject };
    });
  }
}
