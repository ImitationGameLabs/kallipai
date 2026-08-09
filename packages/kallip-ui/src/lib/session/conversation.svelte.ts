// Per-conversation reactive state, drained from a {@link Transport}. One base
// holds the shared surface (transcript, transport status, the two-stream drain,
// the single-in-flight send pump, the history cursor + cache + dedup, and the
// optimistic-line promotion), and the relay leaf carries its transport-specific
// extras (the live/watchdog catch-up gate + background notifications):
//
//   - `RelayConversation` (online): the E2EE pipe; the live gate suppresses
//     notifications during the catch-up batch.
//   - `LocalConversation` (offline): a plain forwarder over the direct SSE.
//
// The two leaves share the `applyReplyCore` reducer path (dedup by `history_id`,
// cache, `user_message` promotion of the optimistic line) and the run() status
// transitions verbatim. Every drain mutation is guarded by an object-identity
// check against the store's live entry for this id (a reconnect replaces the
// entry under the same id; a stale drain must not touch the fresh one).
//
// The offline (store key `"local"`) and online (store key = derived id) entries
// for the SAME tagma share one IndexedDB cache via `cacheConversationId` (the
// tagma's conversation id), so a mode switch rehydrates from the same rows.

import {
  applySignal,
  applyTagmaReply,
  cacheLineOf,
  EMPTY_TRANSCRIPT,
  markLineSent,
  replaceLineId,
  toSender,
  withUserLine,
} from "../transcript.ts";
import type {
  ConversationSender,
  ConversationTranscript,
} from "../transcript.ts";
import type { Participant, TagmaReply } from "@kallipai/kallip-lesche-client";
import { put as cachePut } from "@kallipai/kallip-lesche-client";
import type { Transport } from "./transport.ts";

/** Transport-status surface: the sidebar dot + the chat-page disabled gate. */
export type ConversationStatus = "opening" | "open" | "offline" | "error";

/** The minimal store surface a Conversation needs (back-ref for the stale
 *  guard). Defined here so conversation.svelte.ts does not import the store
 *  module (avoids a cycle). */
export interface ConversationStoreLike {
  get(id: string): ConversationBase | undefined;
}

function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** A synthetic error reply for a POST that failed before the tagma could ack,
 *  routed through the same reducer as a tagma-side error. `req_id` and `status`
 *  are sentinels (the failure did not come from a real reply). */
function syntheticErrorReply(message: string): TagmaReply {
  return { kind: "error", req_id: 0, status: 0, message };
}

export abstract class ConversationBase {
  transcript: ConversationTranscript = $state(EMPTY_TRANSCRIPT);
  status: ConversationStatus = $state("opening");
  /** Transport-level error (the layout banner classifies it). Set when the
   *  drain fails; cleared on a fresh attach. */
  error: unknown = $state(null);
  /** Source of synthetic negative ids for lines without a real history_id.
   *  Plain (the {#each} key is read at line construction, not observed). */
  syntheticSeq = 0;

  abstract readonly kind: "local" | "relay";

  /** The underlying transport, or null once the drain has ended. `$state` so
   *  `connected` (and the store's `localConnected`) re-fire when run() nulls it
   *  on a clean/error drain end -- not just on attach/detach. */
  protected transport = $state<Transport | null>(null);

  /** The IndexedDB cache key (the tagma's conversation id). The offline
   *  `"local"` entry and the online derived-id entry for the same tagma share
   *  this so a mode switch rehydrates from the same rows. Falls back to the
   *  store key when the tagma is not enrolled (no durable history). */
  readonly cacheConversationId: string;
  /** The largest confirmed history_id rendered (the dedup cursor). Any inbound
   *  frame with id <= this is dropped -- unifies catch-up batch and live. */
  maxRendered = $state(0);
  /** Rendered optimistic user lines awaiting their POST. Each entry's line is
   *  already in `transcript` (status "sending"); the single-in-flight send pump
   *  drains this one ack at a time. */
  pending = $state<{ localId: number; text: string }[]>([]);
  /** The ONE in-flight POST (its `user_message` frame has not landed): its
   *  synthetic id + sent text, or null when the pump is idle. The text lets the
   *  promotion branch correlate the echo to this exact send, so a history-replay
   *  `user_message` arriving mid-flight is not mistaken for the ack. */
  pendingInFlight = $state<{ localId: number; text: string } | null>(null);
  /** The latest aggregate status snapshot (root state, subagent counts, token
   *  budget) for THIS conversation's tagma, surfaced to the chat header. The
   *  single uniform source for the header regardless of transport: drained from
   *  `Transport.status()` (direct SSE) for offline, set via
   *  {@link setStatusSnapshot} from the realtime status sink for online (the
   *  relay transport carries no status). `undefined` until the first snapshot. */
  statusSnapshot = $state<
    import("../tagmata.svelte.ts").TagmaStatusSummary | undefined
  >(undefined);

  constructor(
    readonly conversationId: string,
    protected readonly store: ConversationStoreLike,
    transport: Transport | null,
    cacheConversationId: string,
    /** The UI sender for optimistic user lines (online: the agora session;
     *  offline: the tagma-configured local identity). */
    protected readonly localSender: ConversationSender,
  ) {
    this.transport = transport;
    this.cacheConversationId = cacheConversationId;
  }

  get connected(): boolean {
    return this.transport !== null;
  }

  /** Tear down the underlying transport. The run() drain then ends; its stale
   *  guard stops it from mutating the (likely-removed) conversation. */
  close(): void {
    this.transport?.close();
  }

  /** Send a user message. Renders the optimistic line and hands off to the
   *  shared single-in-flight send pump (the in-flight POST's `user_message` frame
   *  promotes the line via `applyReplyCore`). */
  send(text: string): void {
    const trimmed = text.trim();
    if (!this.transport || trimmed === "") return;
    const localId = (this.syntheticSeq -= 1);
    this.transcript = withUserLine(
      this.transcript,
      trimmed,
      localId,
      this.localSender,
    );
    this.pending = [...this.pending, { localId, text: trimmed }];
    void this.pumpPending();
  }

  /** Hook for leaves to react to a reply AFTER the shared core reduce (the relay
   *  flips `live` on `history_batch_end` and fires background notifications). */
  protected onReply(_reply: TagmaReply): void {}

  /** Apply one authored reply through the shared core: dedup by `history_id`,
   *  promote an optimistic line on a stamped `user_message`, reduce, cache, and
   *  advance the cursor. `sender` is the wire participant who authored the
   *  reply's content. Guarded by `isLive` so a stale drain cannot touch a
   *  fresher entry. */
  protected applyReplyCore(
    reply: TagmaReply,
    sender: Participant | undefined,
  ): void {
    // A `user_message` echo closes the in-flight optimistic line -- but only
    // when it is THIS send's echo. Correlate by text: a history-replay
    // `user_message` (e.g. a relay catch-up row) arriving while a fresh local
    // send is in-flight must NOT be consumed as the ack (the echo carries no
    // req_id to correlate on). A stamped echo (history_id > 0) promotes the
    // line to the durable id; an unstamped echo (history_id === 0, a
    // direct-path echo or a DB-write failure) keeps the synthetic id and just
    // flips "sending" -> "sent". Either way the echo is consumed (never
    // appended as a duplicate) and the send pump advances. A non-matching echo
    // falls through to the normal dedup/append path below. Residual edge: a
    // catch-up row whose text identically matches the in-flight send still
    // collides -- far rarer than the prior uncorrelated misfire.
    if (
      reply.kind === "user_message" &&
      this.pendingInFlight !== null &&
      reply.text.trim() === this.pendingInFlight.text.trim()
    ) {
      const localId = this.pendingInFlight.localId;
      const ackId = reply.history_id;
      if (ackId > 0) {
        // The wire sender is authoritative for the confirmed line: overwrite the
        // optimistic line's (stale, client-side) sender so a handle that changed
        // mid-session does not freeze on the old value, matching every other path
        // where the wire sender drives the rendered/cached sender.
        const wireSender = sender ? toSender(sender) : undefined;
        this.transcript = replaceLineId(
          this.transcript,
          localId,
          ackId,
          reply.created_at ?? undefined,
          wireSender,
        );
        const confirmed = this.transcript.lines.find(
          (l) => l.historyId === ackId,
        );
        if (confirmed) {
          // Cache the confirmed user line. Use the plain `wireSender` -- NOT
          // `confirmed.sender`: the line lives in a `$state` transcript, so its
          // nested `sender` is a Svelte Proxy, which IndexedDB's structured clone
          // rejects (DataCloneError). `put` swallows that error, so reading the
          // proxy sender here silently dropped EVERY user message from the cache
          // (they vanished on refresh). `wireSender` is a fresh plain object.
          // `text`/`createdAt` are primitives, safe to read off the proxy line.
          void cachePut({
            conversationId: this.cacheConversationId,
            historyId: ackId,
            role: "user",
            text: confirmed.text,
            sender: wireSender,
            createdAt: confirmed.createdAt,
          });
        }
        if (ackId > this.maxRendered) this.maxRendered = ackId;
      } else {
        // Unstamped echo: keep the synthetic line, flip it to "sent". The echo
        // carries no new content, so drop it (never append) -- otherwise it
        // would render a second bubble and the send pump would never advance.
        this.transcript = markLineSent(this.transcript, localId);
      }
      this.pendingInFlight = null;
      void this.pumpPending();
      this.onReply(reply);
      return;
    }
    // Dedup: a frame at or below the cursor is a replay of something rendered.
    const realId =
      reply.kind === "event" || reply.kind === "user_message"
        ? (reply.history_id ?? 0)
        : 0;
    if (realId > 0 && realId <= this.maxRendered) {
      this.onReply(reply);
      return;
    }
    const lineId = realId > 0 ? realId : (this.syntheticSeq -= 1);
    this.transcript = applyTagmaReply(this.transcript, reply, sender, lineId);
    const cl = cacheLineOf(reply, sender);
    if (cl) {
      void cachePut({
        conversationId: this.cacheConversationId,
        historyId: cl.historyId,
        role: cl.role,
        text: cl.text,
        sender: cl.sender,
        createdAt: cl.createdAt,
      });
      if (cl.historyId > this.maxRendered) this.maxRendered = cl.historyId;
    }
    this.onReply(reply);
  }

  /** Single-in-flight send pump. POSTs the next queued optimistic line (if any,
   *  and if no POST is already outstanding), leaving its localId + text in
   *  `pendingInFlight` so the stamped `user_message` frame can correlate. On a
   *  POST failure, drops the optimistic line and surfaces a synthetic error. */
  protected async pumpPending(): Promise<void> {
    if (this.pendingInFlight !== null) return;
    const next = this.pending.shift();
    if (next === undefined) return;
    this.pendingInFlight = { localId: next.localId, text: next.text };
    try {
      await this.transport!.send(next.text);
    } catch (e) {
      this.transcript = applyTagmaReply(
        {
          ...this.transcript,
          lines: this.transcript.lines.filter(
            (l) => l.historyId !== next.localId,
          ),
        },
        syntheticErrorReply(messageOf(e)),
        undefined,
        (this.syntheticSeq -= 1),
      );
      this.pendingInFlight = null;
      void this.pumpPending();
    }
  }

  /** True iff this conversation is still the store's live entry for its id. */
  protected isLive(): boolean {
    return this.store.get(this.conversationId) === this;
  }

  /** Set the status snapshot from outside the transport drain (the online path:
   *  the realtime feed routes `tagma_status` here via the store). Stale-guarded
   *  so a snapshot for a since-replaced entry cannot resurrect it. */
  setStatusSnapshot(
    snapshot: import("../tagmata.svelte.ts").TagmaStatusSummary | undefined,
  ): void {
    if (!this.isLive()) return;
    this.statusSnapshot = snapshot;
  }

  /** Drain all three transport streams concurrently. Sets status to
   *  error/offline when the transport ends, guarded against a stale drain. */
  async run(): Promise<void> {
    const t = this.transport;
    if (!t) return;
    let failure: unknown = null;
    const drainReplies = async () => {
      try {
        for await (const { sender, reply } of t.replies()) {
          if (!this.isLive()) return;
          this.applyReplyCore(reply, sender);
        }
      } catch (e) {
        if (this.isLive()) failure = e;
      }
    };
    const drainSignals = async () => {
      try {
        for await (const signal of t.signals()) {
          if (!this.isLive()) return;
          this.transcript = applySignal(
            this.transcript,
            signal,
            (this.syntheticSeq -= 1),
          );
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
    } else if (this.status === "opening" || this.status === "open") {
      this.status = "offline";
    }
    this.onDrainDead();
    this.transport = null;
  }

  /** Hook for relay-only cleanup when the drain dies (abandon pending sends). */
  protected onDrainDead(): void {}
}

// ---------------------------------------------------------------------------
// RelayConversation (online)
// ---------------------------------------------------------------------------

/** Fire an OS notification for an inbound authored message when the app is in
 *  the background. Foreground delivery is the transcript itself. */
function maybeNotifyBackground(label: string | null, reply: TagmaReply): void {
  if (typeof Notification === "undefined" || !document.hidden) return;
  if (Notification.permission !== "granted") return;
  if (reply.kind !== "event") return;
  if (reply.event.type !== "assistant_content") return;
  const title = label ? `Tagma ${label}` : "Tagma";
  try {
    new Notification(title, { body: reply.event.content });
  } catch {
    // Some browsers reject construction without a service worker; ignore.
  }
}

export class RelayConversation extends ConversationBase {
  readonly kind = "relay" as const;

  readonly tagmaId: string;
  readonly label: string | null;
  /** True once the initial History batch completed. Before that, inbound frames
   *  are catch-up (old) and must not fire notifications. */
  live = $state(false);
  /** Force-flips `live` after 10s if no history_batch_end arrives (lost-marker
   *  guard). Plain (only the `live` flip it triggers is observed). */
  liveWatchdog: ReturnType<typeof setTimeout> | null = null;

  constructor(
    conversationId: string,
    store: ConversationStoreLike,
    transport: Transport,
    tagmaId: string,
    label: string | null,
  ) {
    // Relay store key == the derived conversation id == the cache key.
    super(
      conversationId,
      store,
      transport,
      conversationId,
      transport.localSender,
    );
    this.tagmaId = tagmaId;
    this.label = label;
  }

  /** The E2EE transport (for the store's history pull, envelope delivery, and
   *  signal routing). */
  get relayTransport(): import("./relayTransport.ts").RelayTransport {
    return this.transport as import("./relayTransport.ts").RelayTransport;
  }

  protected override onReply(reply: TagmaReply): void {
    if (reply.kind === "history_batch_end") {
      if (this.liveWatchdog) {
        clearTimeout(this.liveWatchdog);
        this.liveWatchdog = null;
      }
      this.live = true;
    }
    if (this.live) maybeNotifyBackground(this.label, reply);
  }

  protected override onDrainDead(): void {
    this.abandonPending();
  }

  /** Drop all unsent optimistic state: the in-flight slot, the queued entries,
   *  and their rendered "sending" lines. Used when the channel dies. */
  abandonPending(): void {
    this.pending = [];
    this.pendingInFlight = null;
    this.transcript = {
      ...this.transcript,
      lines: this.transcript.lines.filter((l) => l.status !== "sending"),
    };
  }
}

// ---------------------------------------------------------------------------
// LocalConversation (offline)
// ---------------------------------------------------------------------------

export class LocalConversation extends ConversationBase {
  readonly kind = "local" as const;

  constructor(
    store: ConversationStoreLike,
    transport: Transport,
    cacheConversationId: string,
  ) {
    // attachLocal resolves the transport before binding; there is no opening
    // window. The conversation is open for the moment the drain runs; it flips
    // to offline/error when the SSE ends or fails.
    super(
      "local",
      store,
      transport,
      cacheConversationId,
      transport.localSender,
    );
    this.status = "open";
  }
}
