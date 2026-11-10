// The read path's connection: the transport, the status it reports, the
// carrier the write side rides, and the restart ladder that rebuilds a dead
// direct stream under the same conversation.
//
// The drain lives here because it is the connection's job, but what a frame
// MEANS is not: replies and signals leave through the two callbacks, so this
// file never touches the rows, the queue, or the cache.

import type {
  Participant,
  SignalEvent,
  TagmaReply,
} from "@kallipai/kallipai-lesche-client";
import type { LinkStatus } from "./shared.ts";
import type { Transport } from "../transport.ts";
import type { MessageSender } from "../messageSender.svelte.ts";
import type { TagmaStatusSummary } from "../../tagmata.svelte.ts";
import { statusCardStore } from "../statusCard.svelte.ts";

/** Restart backoff for a dead read path (direct only): the first wait and the
 *  cap the doubling stops at. The stream's own retry ladder covers short gaps;
 *  this covers the case where that ladder gave up. */
export type RestartBackoff = { readonly min: number; readonly max: number };

const RESTART_BACKOFF: RestartBackoff = { min: 15_000, max: 60_000 };

export class Link {
  status = $state<LinkStatus>("opening");

  /** Transport-level error (the layout banner classifies it). Set when the
   *  drain fails; cleared on a fresh attach. */
  error: unknown = $state(null);

  /** The latest aggregate status snapshot (root state, subagent counts, token
   *  budget) for this conversation's tagma, surfaced to the chat header. The
   *  single uniform source for the header regardless of transport: drained from
   *  `Transport.status()` (direct SSE) for offline, set via
   *  {@link setStatusSnapshot} from the realtime status sink for online (the
   *  relay transport carries no status). `undefined` until the first snapshot. */
  statusSnapshot = $state<TagmaStatusSummary | undefined>(undefined);

  /** The underlying transport, or null once the drain ended for good. `$state`
   *  so consumers re-fire when run() nulls it on a terminal drain end -- not
   *  just on attach/detach. A stream death under a restart does not null it. */
  transport = $state<Transport | null>(null);

  /** The write path's carrier: the POST, its settlement and the history pull.
   *  Deliberately not the read-path transport -- a dying stream must not take
   *  the queue with it (see messageSender.svelte.ts). Null only when the
   *  conversation is closed. Reactive, like the sender's own liveness flag:
   *  the write axis changes exactly when a carrier is mounted or its sender
   *  closes, and both fields are `$state` so a reader tracks either flip. */
  sender: MessageSender | null = $state(null);

  /** Set once close() has run: the restart wait and run() both stop here, and
   *  the offline view's canWrite reads it (it has no sender to close). */
  closed = $state(false);

  /** Rebuild the read-path transport after its drain ended. Null = the drain's
   *  end is terminal (the relay, or a conversation held by a test). */
  restartStream: (() => Transport) | null = null;

  /** The rebuild ladder, in milliseconds. Read at every wait and reset, so a
   *  caller can shorten it (a test spending 1ms instead of 15s) without the
   *  drain knowing the difference. */
  backoff: RestartBackoff = { ...RESTART_BACKOFF };

  private restartDelayMs = RESTART_BACKOFF.min;

  /** Resolves the in-flight restart wait early: a close cuts it short. */
  private restartWake: (() => void) | null = null;

  constructor(
    /** True while this conversation is still its store's live entry. */
    private readonly isLive: () => boolean,
    /** The leaf's share of the write axis: does the path a line travels exist
     *  right now (see ConversationBase.carrierLive). */
    private readonly carrierLive: () => boolean,
    /** One inbound reply, already stale-guarded. */
    private readonly onReply: (
      reply: TagmaReply,
      sender: Participant | undefined,
    ) => void,
    /** One inbound status signal, already stale-guarded. */
    private readonly onSignal: (signal: SignalEvent) => void,
    /** The leaf's stream-lifecycle hook (first attach and every rebuild). */
    private readonly onAttached: () => void,
    /** The leaf's cleanup when the drain ends for good. */
    private readonly onDead: () => void,
  ) {}

  /** The write axis: whether a line typed now has a path to land. Two inputs:
   *  the conversation is open, and this leaf's carrier exists. Both are runes
   *  (the fields behind them are `$state`), so a page deriving from this
   *  re-evaluates on close and on a carrier changing under it. A stream
   *  rebuilding leaves both true; `connected` adds stream health. */
  get canWrite(): boolean {
    return !this.closed && this.carrierLive();
  }

  /** True while the conversation holds its destination: writable, a stream
   *  holder, and a stream that is not reconnecting. Both layers report the
   *  same fact on `status`: the transport's own SSE retry says "reconnecting"
   *  through onTransportState, and a drain end under a restart says it in
   *  run(). A stream death is not a disconnect, so the status -- not the
   *  transport's presence -- is what flips this false. The relay panel's
   *  channel window is the same predicate: its drain end is terminal, so it
   *  nulls the transport rather than going "reconnecting". Built on canWrite
   *  so the two cannot drift: this is the lamp, that is the write axis. */
  get connected(): boolean {
    return (
      this.canWrite && this.transport !== null && this.status !== "reconnecting"
    );
  }

  /** Tear down the underlying transport. The run() drain then ends; its stale
   *  guard stops it from mutating the (likely-removed) conversation. */
  close(): void {
    this.transport?.close();
    this.sender?.close();
    this.closed = true;
    this.restartWake?.();
  }

  /** Set the status snapshot from outside the transport drain (the online path:
   *  the realtime feed routes `tagma_status` here via the store). Stale-guarded
   *  so a snapshot for a since-replaced entry cannot resurrect it. */
  setStatusSnapshot(snapshot: TagmaStatusSummary | undefined): void {
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
          this.onReply(reply, sender);
        }
      } catch (e) {
        if (this.isLive()) failure = e;
      }
    };
    const drainSignals = async () => {
      try {
        for await (const signal of t.signals()) {
          if (!this.isLive()) return;
          this.onSignal(signal);
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
      // The shell banner owns this error on the local surface; a relay
      // conversation has no such owner, so record it where it is set.
      console.error(failure);
    } else if (this.status === "opening" || this.status === "open") {
      this.status = "offline";
    }
  }

  /**
   * Drain the read path, rebuilding it under the same sender when it dies.
   *
   *  The direct stream is replaceable (a fresh transport over the same client and
   *  agent), so a dead stream is stream maintenance, not the end of the session:
   *  the wait is bounded by a backoff the conversation owns, and a close cuts
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
        this.onDead();
        this.transport = null;
        return;
      }
      this.status = "reconnecting";
      const rebuilt = await this.waitForRestart();
      if (rebuilt === null) return;
      this.transport = rebuilt;
      this.onAttached();
    }
  }

  /** Wait out the restart backoff, cut short by {@link close}. Returns the
   *  rebuilt transport, or null when the conversation stopped being live. */
  private async waitForRestart(): Promise<Transport | null> {
    // The step clamps into the current bounds, so replacing the ladder also
    // shortens a wait that is already scheduled.
    const delay = Math.min(
      Math.max(this.restartDelayMs, this.backoff.min),
      this.backoff.max,
    );
    this.restartDelayMs = Math.min(delay * 2, this.backoff.max);
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

  /** Reset the restart backoff: a live stream means the next interruption
   *  starts at the first wait again. */
  resetRestartBackoff(): void {
    this.restartDelayMs = this.backoff.min;
  }
}
