// The online (relayed) transport: adapts an E2EE {@link RelayChannel} (the
// authored-content pipe) plus a signal queue (fed by the realtime SSE's
// `tagma_signal` dispatch) to the {@link Transport} seam.
//
// Replies (acks, errors, replayed user messages, authored `assistant_content`)
// come straight off the RelayChannel. Signals arrive on a SEPARATE process-wide
// SSE owned by realtimeStore; the store routes each `tagma_signal` to the owning
// conversation's transport via {@link enqueueSignal}, and the conversation's
// `signals()` drain consumes them here -- so the conversation drains replies +
// signals from one uniform interface regardless of transport.

import type { RelayChannel, SignalEvent } from "@kallipai/kallip-lesche-client";
import type { TagmaStatusSummary } from "../tagmata.svelte.ts";
import type { ConversationSender } from "../transcript.ts";
import { toSender } from "../transcript.ts";
import { AsyncQueue, type IncomingFrame, type Transport } from "./transport.ts";

export class RelayTransport implements Transport {
  private readonly signalQueue = new AsyncQueue<SignalEvent>();
  readonly localSender: ConversationSender;

  constructor(private readonly channel: RelayChannel) {
    this.localSender = toSender(channel.localParticipant);
  }

  async *status(): AsyncGenerator<TagmaStatusSummary> {
    // Empty by design: the relay's status rides the agora SSE (the
    // `tagma_status` LescheEvent), routed into the conversation via the realtime
    // status sink -- not this E2EE transport. The uniform Transport.status()
    // drain in Conversation.run() therefore contributes nothing here.
  }

  async *replies(): AsyncGenerator<IncomingFrame> {
    // The reply stream ending or throwing means the E2EE channel is dead. The
    // signal stream is a separate queue (fed by the realtime SSE); close it here
    // so the Conversation's signals drain ends too -- otherwise its run()
    // (Promise.allSettled) would wait forever for a signal drain blocked on a
    // never-closing queue.
    try {
      for await (const r of this.channel.replies()) yield r;
    } finally {
      this.signalQueue.close();
    }
  }

  async *signals(): AsyncGenerator<SignalEvent> {
    while (true) {
      const { value, done } = await this.signalQueue.next();
      if (done) return;
      yield value;
    }
  }

  /** Push a runtime signal routed by the realtime feed. Called by the store's
   *  signal dispatch (keyed by tagma id). */
  enqueueSignal(event: SignalEvent): void {
    this.signalQueue.push(event);
  }

  send(text: string): Promise<void> {
    return this.channel.send(text);
  }

  /** The underlying E2EE pipe. Exposed for the two non-stream ops the store
   *  needs past the Transport interface: `history()` (the catch-up pull, result
   *  flows back via `replies`) and `enqueue(envelope)` (the realtime envelope
   *  delivery). Reach for nothing else here -- call `close()` on the transport,
   *  not on the channel, so the signal queue is torn down too. */
  get relayChannel(): RelayChannel {
    return this.channel;
  }

  /** Detach from the relay: close the E2EE channel and the signal queue. The
   *  underlying agora SSE is owned by realtimeStore and is not torn down here
   *  (it is shared across all conversations). */
  close(): void {
    this.channel.close();
    this.signalQueue.close();
  }
}
