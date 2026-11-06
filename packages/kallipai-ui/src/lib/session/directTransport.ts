// The offline (local, non-relay) transport: serves the tagma's external
// chat-room API over a plain HTTP+SSE connection with no relay process and no
// E2EE. It is the frontend's sole window onto a directly-connected tagma --
// authored assistant messages, runtime signals (busy/idle presence, turn
// terminals, errors), and status snapshots all arrive on the one multiplexed
// SSE, demuxed here by the `event:` field name.
//
// A lagged subscriber gets a `missed` frame in place of the frames it lost:
// the marker carries no payload, because the reader's own cursor already says
// where it stopped, and it repairs the hole with a history pull from there.
//
// The Transport contract exposes three streams (`replies` + `signals` +
// `status`) but the direct wire is one SSE, so an internal drain fans each
// decoded frame out into three queues. The direct path has no
// `message_accepted` ack: the user's inbound POST resolves synchronously with
// no `history_id`, so the store renders the optimistic user line as sent once
// the POST resolves.
import { TransportError } from "@kallipai/kallipai-common";

import type { TagmaClient } from "@kallipai/kallipai-client";
import type {
  Participant,
  SignalEvent,
  TagmaReply,
} from "@kallipai/kallipai-lesche-client";
import type { TagmaStatusSummary } from "../tagmata.svelte.ts";
import type { ConversationSender } from "../transcript.ts";
import { AsyncQueue, type IncomingFrame, type Transport } from "./transport.ts";

/** The aggregate runtime snapshot on the direct stream (the snake_case wire
 * shape, mirroring the relay's `tagma_status` LescheEvent). Ephemeral operator
 * metadata; the status header renders it. The transcript is driven by signals,
 * not by these snapshots. */
export interface DirectStatusPayload {
  readonly root_state:
    | "idle"
    | "busy"
    | "waiting"
    | "retrying"
    | "parked"
    | "faulted";
  readonly subagents_total: number;
  readonly subagents_active: number;
  readonly token_budget: number;
  readonly token_consumed: number;
  // Unlimited budget (enforcement off, consumption still tracked); absent
  // from older tagmas (serde default false).
  readonly token_budget_unlimited?: boolean;
}

/** The `authored` SSE data payload: the sender paired with the content reply
 * (the offline path has no relay envelope, so the direct frame carries the
 * sender itself). Mirrors the Rust `DirectAuthoredPayload`. */
export interface DirectAuthoredPayload {
  readonly sender: Participant;
  readonly reply: TagmaReply;
}

/** One frame on the direct external SSE, discriminated by the SSE `event:`
 * field. Authored content is persisted/replayable on the tagma; signals and
 * status are ephemeral; `missed` replaces the frames a lagged subscriber lost;
 * closing says the tagma is shutting down and nothing follows. */
export type DirectFrame =
  | { readonly kind: "authored"; readonly payload: DirectAuthoredPayload }
  | { readonly kind: "signal"; readonly event: SignalEvent }
  | { readonly kind: "status"; readonly payload: DirectStatusPayload }
  | { readonly kind: "missed" }
  | { readonly kind: "closing" };

/** Map the snake_case direct status payload to the transport-neutral
 *  `TagmaStatusSummary` (the same shape the relay path's archeion-SSE status maps
 *  to), so the conversation drains one uniform type from either transport. */
function toSummary(p: DirectStatusPayload): TagmaStatusSummary {
  return {
    rootState: p.root_state,
    subagentsTotal: p.subagents_total,
    subagentsActive: p.subagents_active,
    tokenBudget: p.token_budget,
    tokenConsumed: p.token_consumed,
    tokenBudgetUnlimited: p.token_budget_unlimited ?? false,
  };
}

/** Backoff between SSE reconnect attempts (the transport-level retry loop):
 * four attempts span ~15s of silent retrying before the failure is surfaced
 * as final — the user sees the in-chat reconnecting spinner, never an error.
 * Any body bytes reset the budget. */
const STREAM_RETRY_DELAYS_MS = [1000, 2000, 4000, 8000] as const;

/** The tagma's SSE keepalive cadence: the handler wraps every stream in
 * `Sse::keep_alive(KeepAlive::default())`, which writes a `:` comment every
 * 15s (crates/kallipai-tagma/src/sse.rs:227). Liveness is those bytes, not
 * parsed events. */
const SSE_KEEPALIVE_HINT_MS = 15_000;

/** No body bytes (the keepalive comments above are bytes on the wire even
 * though they carry no event) for this long means the connection is half-open:
 * tear it down and reconnect. Three keepalive intervals absorb jitter, so the
 * interval can never drop below twice the keepalive cadence. */
const STREAM_WATCHDOG_MS = 3 * SSE_KEEPALIVE_HINT_MS;

/** Stream lifecycle states surfaced to the owning conversation: the retry
 * loop is taking over ("reconnecting") and a fresh connection is live
 * ("resumed" — the conversation backfills the gap via catch-up). */
export type TransportState = "reconnecting" | "resumed";

/** Why a stream attempt ended that was not a failure: the watchdog tore
 * down a half-open socket, the app came back to the foreground, or the
 * tagma said it is shutting down. None of them spend the retry budget. */
type RestartKind = "watchdog" | "foreground" | "planned";

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
  private readonly replyQueue = new AsyncQueue<IncomingFrame>();
  private readonly signalQueue = new AsyncQueue<SignalEvent>();
  private readonly statusQueue = new AsyncQueue<TagmaStatusSummary>();
  private muxStarted = false;
  /** Stream lifecycle notifications for the owning conversation, wired by
   * the store at attach: "reconnecting" when the retry loop takes over,
   * "resumed" once a fresh connection is live. Null drops the event. */
  onState: ((s: TransportState) => void) | null = null;
  /** A lagged subscriber's repair marker: frames were lost, so the reader
   *  resumes from its own position cursor. Wired by the store, which pulls the
   *  hole back through the history cursor; null drops the marker. */
  onMissed: (() => void) | null = null;
  /** Abort for the CURRENT stream attempt only; the transport's own
   * controller stays reserved for user-initiated close. */
  private streamCtl: AbortController | null = null;
  private watchdog: ReturnType<typeof setTimeout> | null = null;
  /** Resolves the in-flight reconnect backoff early (foreground fast path). */
  private kickSleep: (() => void) | null = null;
  /** Why the current stream attempt ended: null = organic end/failure;
   * otherwise a maintenance restart (see [`RestartKind`]). */
  private restartKind: RestartKind | null = null;
  private readonly streamRetryDelays: readonly number[];
  private readonly watchdogMs: number;

  constructor(
    private readonly client: TagmaClient,
    readonly agentId: string,
    readonly localSender: ConversationSender,
    /** SSE reconnect backoff table (test seam; production uses the module
     * default). */
    streamRetryDelays: readonly number[] = STREAM_RETRY_DELAYS_MS,
    /** Half-open watchdog interval in ms (test seam). */
    watchdogMs = STREAM_WATCHDOG_MS,
  ) {
    this.streamRetryDelays = streamRetryDelays;
    this.watchdogMs = watchdogMs;
  }

  async *replies(): AsyncGenerator<IncomingFrame> {
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

  close(): void {
    this.controller.abort();
    this.streamCtl?.abort(); // cut the in-flight stream attempt too
    this.kickSleep?.(); // release the retry loop from its backoff sleep
    this.disarmWatchdog();
    this.unbindForeground();
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
    this.bindForeground();
    let attempt = 0;
    while (!this.controller.signal.aborted) {
      this.restartKind = null;
      this.streamCtl = new AbortController();
      let failure: unknown = null;
      try {
        // The first liveness tick (connection open) both feeds the watchdog
        // and reports "resumed" — the conversation then backfills whatever
        // the reconnect gap dropped via its cursor-based catch-up. Every body
        // chunk feeds it from then on, keepalive comments included.
        let live = false;
        // Liveness is byte arrival, so a stream whose event source stalls
        // but whose keepalives still flow stays connected: this watchdog
        // catches a dead socket, not a stuck producer.
        const onLiveness = () => {
          this.feedWatchdog();
          if (!live) {
            live = true;
            attempt = 0; // a live connection proves the stream is healthy.
            this.onState?.("resumed");
          }
        };
        this.armWatchdog();
        for await (const f of this.frames(this.streamCtl.signal, onLiveness)) {
          if (f.kind === "closing") {
            // The tagma is shutting down: leave now instead of waiting for
            // the body to end (a middlebox or a stuck writer could hold it
            // open). The reconnect is immediate stream maintenance, with no
            // floor for repeated closes: a single attempt carries no signal
            // to key one on (the connect snapshot always precedes the mark),
            // and a tagma that stays down refuses new connections, which
            // spends the retry ladder instead.
            this.restartKind = "planned";
            this.streamCtl?.abort();
            break;
          }
          switch (f.kind) {
            case "authored":
              this.replyQueue.push({
                sender: f.payload.sender,
                reply: f.payload.reply,
              });
              break;
            case "signal":
              this.signalQueue.push(f.event);
              break;
            case "status":
              this.statusQueue.push(toSummary(f.payload));
              break;
            case "missed":
              this.onMissed?.();
              break;
          }
        }
      } catch (e) {
        failure = e;
      } finally {
        this.disarmWatchdog();
      }
      if (this.controller.signal.aborted) break; // user close: queues closed
      // A proactive restart (watchdog / foreground return) and a planned
      // close (the server leaving) are stream maintenance, not failures:
      // reconnect without spending the retry budget.
      const kind = this.takeRestartKind();
      if (kind !== null) {
        if (kind === "watchdog") {
          console.error(
            "[sse] no bytes for " +
              this.watchdogMs +
              "ms (half-open?), reconnecting",
          );
          this.onState?.("reconnecting");
        }
        continue;
      }
      attempt += 1;
      const budget = this.streamRetryDelays.length;
      if (failure !== null) {
        console.error(
          "[sse] stream failed (attempt " + attempt + "/" + budget + "): ",
          failure,
        );
      } else {
        // A clean server close still reconnects: mobile backgrounding often
        // surfaces as a clean close, and a truly stopped tagma exhausts the
        // same budget and surfaces the same final banner.
        console.error(
          "[sse] stream ended cleanly (attempt " +
            attempt +
            "/" +
            budget +
            "), reconnecting",
        );
      }
      if (attempt > budget) {
        const e = failure ?? new TransportError("stream closed");
        this.replyQueue.fail(e);
        this.signalQueue.fail(e);
        this.statusQueue.fail(e);
        this.unbindForeground();
        return;
      }
      this.onState?.("reconnecting");
      await this.waitRestart(this.streamRetryDelays[attempt - 1]!);
    }
    this.unbindForeground();
    this.replyQueue.close();
    this.signalQueue.close();
    this.statusQueue.close();
  }

  /** Reconnect backoff, interruptible by the foreground fast path (which
   * swaps the stream at once) and by close(). */
  private waitRestart(ms: number): Promise<void> {
    return new Promise((resolve) => {
      const done = () => {
        clearTimeout(timer);
        this.kickSleep = null;
        resolve();
      };
      const timer = setTimeout(done, ms);
      this.kickSleep = done;
    });
  }

  /** No body bytes (keepalive comments count) for watchdogMs: the connection
   * is half-open. Abort the attempt; the retry loop reconnects immediately
   * without spending the budget. The timer only re-arms from a pull of the
   * stream, which is synchronous in the mux loop today (as is `AsyncQueue`'s
   * push), so a slow consumer cannot starve it. */
  private armWatchdog(): void {
    this.disarmWatchdog();
    this.watchdog = setTimeout(() => {
      this.restartKind = "watchdog";
      this.streamCtl?.abort();
    }, this.watchdogMs);
  }

  /** The reason this attempt ended as maintenance, cleared on read. */
  private takeRestartKind(): RestartKind | null {
    const kind = this.restartKind;
    this.restartKind = null;
    return kind;
  }

  private feedWatchdog(): void {
    if (this.watchdog !== null) this.armWatchdog(); // re-arm on liveness
  }

  private disarmWatchdog(): void {
    if (this.watchdog !== null) {
      clearTimeout(this.watchdog);
      this.watchdog = null;
    }
  }

  /** Foreground fast path: coming back from the background, the stream is
   * likely half-open (the OS froze the socket). Tear it down and reconnect
   * at once — also cutting any in-flight backoff sleep. Public for tests;
   * bound to visibilitychange where the DOM exists. */
  handleForegroundVisible(): void {
    const visible =
      typeof document === "undefined" || document.visibilityState === "visible";
    if (!visible) return;
    this.restartKind = "foreground";
    this.kickSleep?.();
    this.streamCtl?.abort();
  }

  private bindForeground(): void {
    if (typeof document === "undefined") return;
    document.addEventListener("visibilitychange", this.handleForegroundVisible);
  }

  private unbindForeground(): void {
    if (typeof document === "undefined") return;
    document.removeEventListener(
      "visibilitychange",
      this.handleForegroundVisible,
    );
  }

  /** Iterate the multiplexed external SSE, yielding decoded frames until the
   *  stream ends or {@link close} aborts it. A malformed payload is dropped
   *  rather than killing the stream (one bad frame must not lose the
   *  connection). */
  async *frames(
    signal = this.controller.signal,
    onLiveness?: () => void,
  ): AsyncGenerator<DirectFrame> {
    for await (const f of this.client.externalEventStream(
      this.agentId,
      signal,
      onLiveness,
    )) {
      try {
        switch (f.event) {
          case "authored":
            yield {
              kind: "authored",
              payload: JSON.parse(f.data) as DirectAuthoredPayload,
            };
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
          case "missed":
            // No payload: the reader's own cursor says where to resume.
            yield { kind: "missed" };
            break;
          case "closing":
            yield { kind: "closing" };
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
