// ChannelsStore: the online-mode, per-tagma chat state. The independent online
// counterpart of SessionStore (session.svelte.ts) -- it does NOT reuse the
// offline Session/applyEvent; it has its own transcript reducer
// (../channel/transcript.ts). Each open channel owns a RelayChannel (the Phase-1
// E2EE transport).
//
// Inbound envelopes arrive via realtime.svelte.ts (the single shared SSE
// subscriber), which routes each by conversation id into `deliver` here. The
// agora client supplies the pinned key (getTagma) and the lesche client runs the
// key exchange + envelope relay (see openRelayChannel). Both are injected
// singletons from agora.svelte.ts.

import { type TagmaView } from "@kallipai/kallip-agora-client";
import {
  type Envelope,
  openRelayChannel,
  type RelayChannel,
  type TagmaReply,
  clearConvCache,
  loadAll,
  put as cachePut,
} from "@kallipai/kallip-lesche-client";
import { SvelteMap } from "svelte/reactivity";
import {
  agoraClientOrFail,
  agoraSession,
  lescheClientOrFail,
} from "./agora.svelte.ts";
import {
  applyTagmaReply,
  cacheLineOf,
  type ChannelLine,
  type ChannelTranscript,
  EMPTY_TRANSCRIPT,
  replaceLineId,
  withUserLine,
} from "../channel/transcript.ts";
import type { NavIndicator } from "../shell.ts";

function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** Fire an OS notification for an inbound content event when the app is in the
 * background (`document.hidden`) — the "agent messaged while you were away"
 * surface. Foreground delivery is the transcript itself (no toast spam). The
 * Web Push extension (app fully closed) is Phase 4; this covers the background
 * tab. NOTE: notification permission is NOT auto-requested here — it must be
 * granted via a user-gesture surface (shell wiring is a follow-up); until then
 * `permission === "default"` and this is a silent no-op. */
function maybeNotifyBackground(label: string | null, reply: TagmaReply): void {
  if (typeof Notification === "undefined" || !document.hidden) return;
  if (Notification.permission !== "granted") return;
  if (reply.kind !== "event") return;
  const event = reply.event;
  let body: string | null = null;
  if (event.type === "assistant_content") body = event.content;
  else if (event.type === "status") body = event.message;
  if (!body) return;
  const title = label ? `Tagma ${label}` : "Tagma";
  try {
    new Notification(title, { body });
  } catch {
    // Some browsers reject construction without a service worker; ignore.
  }
}

/** Build a synthetic `error` reply from a thrown exception, so a send failure
 * routes through the same reducer as a tagma-side error. `req_id` and
 * `status` are sentinels (the failure did not originate from a tagma reply). */
function syntheticErrorReply(message: string): TagmaReply {
  return { kind: "error", req_id: 0, status: 0, message };
}

/** Map a channel's transport status to a sidebar dot. This is OUR channel
 * transport (the KEX/drain lifecycle), distinct from the dashboard's
 * `TagmaPresence` / `realtimeStore` presence, which is the PEER presence (a
 * tagma tunnel is live). `error` (KEX/drain failure) is kept distinct from
 * `offline` (peer went away) so the sidebar can flag "click to retry" vs
 * "asleep". */
function channelIndicator(
  status: "opening" | "open" | "offline" | "error",
): NavIndicator {
  switch (status) {
    case "open":
      return "live";
    case "opening":
      return "pending";
    case "offline":
      return "down";
    case "error":
      return "error";
  }
}

/** One open channel's reactive state. A rune class so each field is reactive on
 * its own (class instances are not deep-proxied by $state). */
export class ChannelState {
  transcript: ChannelTranscript = $state(EMPTY_TRANSCRIPT);
  /** Rendered optimistic user lines awaiting their POST. Each entry's line is
   * already in `transcript` (status `"sending"`); the single-in-flight send
   * pump drains this one ack at a time. */
  pending: { localId: number; text: string }[] = $state([]);
  status: "opening" | "open" | "offline" | "error" = $state("opening");
  // The transport; plain (not reactive). Set once openRelayChannel resolves.
  channel: RelayChannel | null = null;
  /** The largest confirmed `history_id` rendered (the cursor). Any inbound
   * frame with `history_id <= maxRendered` is dropped — this unifies dedup
   * across the catch-up batch and live delivery. Persisted implicitly via the
   * local cache (the max id it holds). */
  maxRendered: number = $state(0);
  /** True once the initial catch-up `History` batch has completed
   * (`history_batch_end` received). Before that, inbound frames are catch-up
   * (old) and must not fire notifications; after, they are live. */
  live: boolean = $state(false);
  /** The synthetic negative id of the ONE in-flight POST (its ack has not yet
   * landed), or `null` when the pump is idle. Single-in-flight so the
   * `MessageAccepted` ack unambiguously correlates to this line. */
  pendingLocalId: number | null = $state(null);
  /** Source of synthetic negative ids for lines without a real `history_id`
   * (optimistic user lines, ack-class errors). Decremented per use; not
   * reactive (the {#each} key is read at line construction, not observed). */
  syntheticSeq: number = 0;
  /** Deadline timer that force-flips `live=true` if no `history_batch_end`
   * arrives within ~10s of `open()`. Guards the partial-delivery / lost-marker
   * case: if the tagma drops a batch mid-way (it deliberately omits the marker
   * on partial delivery so the app retries on reconnect), or the marker POST is
   * lost, or the relay crashes, `live` would otherwise stay false for the whole
   * session and silently disable background notifications. The normal path
   * (`batch_end` arriving) cancels this; the timer is defense-in-depth. Plain
   * (not reactive): only the `live` flip it triggers is observed. */
  liveWatchdog: ReturnType<typeof setTimeout> | null = null;

  constructor(
    readonly conversationId: string,
    readonly tagmaId: string,
    readonly label: string | null,
  ) {}
}

class ChannelsStore {
  /** conversationId -> channel state. `SvelteMap` (not `$state(new Map())`):
   * Svelte's `$state` proxy does not wrap Map/Set, so a raw Map's in-place
   * `.set()` would be invisible to reactivity and the sidebar would never
   * update. SvelteMap tracks membership + iteration natively; per-field
   * reactivity of each entry still comes from ChannelState's own runes. */
  private channels = new SvelteMap<string, ChannelState>();

  /** Snapshot for the sidebar: conversationId/label/indicator per open channel.
   * Reading `c.status` here also subscribes the indicator to mid-life status
   * changes (e.g. drain flipping open -> offline). */
  get list(): {
    conversationId: string;
    label: string | null;
    indicator: NavIndicator;
  }[] {
    return Array.from(this.channels.values()).map((c) => ({
      conversationId: c.conversationId,
      label: c.label,
      indicator: channelIndicator(c.status),
    }));
  }

  get(conversationId: string): ChannelState | undefined {
    return this.channels.get(conversationId);
  }

  /**
   * Open an E2EE channel to `tagma`. Runs the key exchange, hydrates the
   * transcript from the local cache (instant), then asks the tagma for an
   * incremental history batch (`after: maxRendered`) — or the most recent
   * window if the cache is empty — and drains the reply stream. Resolves to the
   * conversation id. Throws if the user is unsigned or KEX fails.
   */
  async open(tagma: TagmaView): Promise<string> {
    const userId = agoraSession.user?.user_id;
    if (!userId) throw new Error("not signed in");

    // KEX is synchronous HTTP; inbound replies flow only through realtime's SSE
    // demux into `deliver`, which begins well before the user can send. The
    // pinned device key is TOFU from the agora (control plane); the lesche
    // client takes it as a base64 string so it has no agora dependency.
    const info = await agoraClientOrFail().getTagma(tagma.tagma_id);
    const channel = await openRelayChannel(
      lescheClientOrFail(),
      tagma.tagma_id,
      userId,
      info.pinned_public_key,
    );
    const state = new ChannelState(
      channel.conversationId,
      tagma.tagma_id,
      tagma.label,
    );
    state.channel = channel;
    // Hydrate from the per-device cache before marking open, so a refresh
    // restores the conversation instantly and we only pull a delta.
    const cached = await loadAll(channel.conversationId);
    if (cached.length > 0) {
      const lines: ChannelLine[] = cached.map(({ historyId, role, text }) => ({
        historyId,
        // The cache stores role as an opaque string (it is a UI concept); cast
        // back to the reducer's role union, which the values round-trip as.
        role: role as ChannelLine["role"],
        text,
      }));
      state.transcript = { lines, status: "idle" };
      state.maxRendered = cached[cached.length - 1]!.historyId;
    }
    state.status = "open";
    this.channels.set(channel.conversationId, state);
    // Pull what we are missing: incremental if the cache had state, else the
    // most recent window. Drained through the normal reply stream.
    const after = state.maxRendered > 0 ? state.maxRendered : null;
    try {
      await channel.history({ after, limit: 50 });
    } catch {
      // Non-fatal: live delivery still works; the next open retries the pull.
      // Flip `live` on anyway -- with no catch-up batch in flight, subsequent
      // frames are live and must be allowed to notify (otherwise a failed pull
      // would silently disable background notifications for the whole session).
      state.live = true;
    }
    // Watchdog: if no `history_batch_end` lands within 10s (partial delivery,
    // lost marker, relay crash), force `live` so background notifications still
    // fire for this session. The `batch_end` handler in `applyReply` cancels
    // this timer on the normal path. Object-identity guard so a stale closure
    // (a second `open()` of the same conv replaced the map entry) no-ops. Arm
    // BEFORE kicking drain so an early `batch_end` finds the handle to clear.
    state.liveWatchdog = setTimeout(() => {
      if (this.channels.get(channel.conversationId) === state && !state.live) {
        state.live = true;
      }
    }, 10_000);
    void this.drain(channel.conversationId);
    return channel.conversationId;
  }

  /** Send a prompt. Renders the optimistic user line immediately (so it is
   * visible before the turn even starts), enqueues it, and lets the
   * single-in-flight pump POST it. Busy/idle is irrelevant -- the tagma accepts
   * mid-turn messages and may coalesce them; each message is its own send with
   * its own ack. */
  send(conversationId: string, text: string): Promise<void> {
    const ch = this.channels.get(conversationId);
    const trimmed = text.trim();
    if (!ch || !ch.channel || trimmed === "") return Promise.resolve();
    const localId = (ch.syntheticSeq -= 1);
    ch.transcript = withUserLine(ch.transcript, trimmed, localId);
    ch.pending = [...ch.pending, { localId, text: trimmed }];
    void this.pumpPending(ch);
    return Promise.resolve();
  }

  /** Close + drop a channel. Drops its IndexedDB cache too, so a conv that is
   * closed (and thus absent from the map at logout) does not leave plaintext
   * behind on a shared device. */
  close(conversationId: string): void {
    const ch = this.channels.get(conversationId);
    if (ch?.liveWatchdog) clearTimeout(ch.liveWatchdog);
    ch?.channel?.close();
    this.channels.delete(conversationId);
    void clearConvCache(conversationId);
  }

  /** Route an inbound envelope (handed off by realtime.svelte.ts's SSE demux)
   * to the channel that owns its conversation. Unknown ids are dropped -- the
   * envelope belongs to a channel the app has not opened. */
  deliver(envelope: Envelope): void {
    this.channels.get(envelope.conversation_id)?.channel?.enqueue(envelope);
  }

  /** Tear down every open channel. Called by the shell on logout and on leaving
   * online mode. The SSE subscriber is owned by realtime.svelte.ts and torn down
   * separately; this closes the per-channel transports, clears the map, and
   * purges the per-conversation IndexedDB cache so a shared device does not
   * retain the previous user's plaintext transcript. */
  reset(): void {
    const conversationIds = Array.from(this.channels.keys());
    for (const ch of this.channels.values()) {
      if (ch.liveWatchdog) clearTimeout(ch.liveWatchdog);
      ch.channel?.close();
    }
    this.channels.clear();
    for (const conversationId of conversationIds) {
      void clearConvCache(conversationId);
    }
  }

  // --- internals -----------------------------------------------------------

  /** Single-in-flight send pump. POSTs the next queued optimistic line (if any,
   * and if no POST is already outstanding) and leaves its `localId` in
   * `pendingLocalId` so the `MessageAccepted` ack can correlate. Self-driving:
   * called from `send` (on enqueue) and the ack handler (on promotion). */
  private async pumpPending(ch: ChannelState): Promise<void> {
    if (ch.pendingLocalId !== null) return;
    const next = ch.pending.shift();
    if (next === undefined) return;
    ch.pendingLocalId = next.localId;
    // The optimistic line was rendered in `send`; here we only POST it.
    try {
      await ch.channel!.send(next.text);
    } catch (e) {
      // POST failed before the tagma could ack: drop the optimistic line and
      // surface the failure through the same reducer path as a tagma-side
      // error. Do not auto-pump -- a POST failure usually means the channel is
      // broken; the next send/ack resumes the pump once it recovers.
      ch.transcript = applyTagmaReply(
        {
          ...ch.transcript,
          lines: ch.transcript.lines.filter(
            (l) => l.historyId !== next.localId,
          ),
        },
        syntheticErrorReply(messageOf(e)),
        (ch.syntheticSeq -= 1),
      );
      ch.pendingLocalId = null;
    }
  }

  /** Drain a channel's reply stream into its transcript. Ends when the channel
   * is closed (replies generator ends); the channel then reads as offline. */
  private async drain(conversationId: string): Promise<void> {
    const ch = this.channels.get(conversationId);
    if (!ch?.channel) return;
    const channel = ch.channel;
    try {
      for await (const reply of channel.replies()) {
        this.applyReply(conversationId, ch, reply);
      }
    } catch {
      if (this.channels.get(conversationId) === ch) {
        ch.status = "error";
        // Channel is dead: the in-flight POST and any queued optimistic lines
        // can never send. Abandon them so the badge stops showing "queued",
        // the pump does not wedge, and the UI does not leave them pulsing.
        this.abandonPending(ch);
      }
    } finally {
      if (this.channels.get(conversationId) === ch && ch.status === "open") {
        ch.status = "offline";
        this.abandonPending(ch);
      }
    }
  }

  /** Drop all unsent optimistic state: the in-flight slot, the queued pending
   * entries, and their rendered `"sending"` lines. Used when the channel dies
   * (error/offline) -- none of those messages can ever deliver, so leaving them
   * would mislead (a stuck pulse, a stale "queued" badge). The channel-status
   * banner is the surfaced signal. */
  private abandonPending(ch: ChannelState): void {
    ch.pending = [];
    ch.pendingLocalId = null;
    ch.transcript = {
      ...ch.transcript,
      lines: ch.transcript.lines.filter((l) => l.status !== "sending"),
    };
  }

  /** Apply one reply to the transcript + cache + cursor. Dedup is by
   * `maxRendered` and unifies catch-up batch frames and live frames; the
   * optimistic user line is promoted to its real id on ack. Notifications fire
   * only for live content (after the catch-up batch completes), when the app is
   * backgrounded. */
  private applyReply(
    conversationId: string,
    ch: ChannelState,
    reply: TagmaReply,
  ): void {
    const realId =
      reply.kind === "event" || reply.kind === "user_message"
        ? (reply.history_id ?? 0)
        : 0;
    // Dedup: a content frame already rendered (catch-up + live share this) is
    // dropped. Frames with no real id (synthetic) always pass through.
    if (realId > 0 && realId <= ch.maxRendered) return;
    const lineId = realId > 0 ? realId : (ch.syntheticSeq -= 1);
    ch.transcript = applyTagmaReply(ch.transcript, reply, lineId);
    // Cache + advance the cursor for content with a real id.
    const cl = cacheLineOf(reply);
    if (cl) {
      void cachePut({
        conversationId,
        historyId: cl.historyId,
        role: cl.role,
        text: cl.text,
      });
      if (cl.historyId > ch.maxRendered) ch.maxRendered = cl.historyId;
    }
    // The `MessageAccepted` ack closes the in-flight send. When it carries a
    // real `history_id` (> 0), promote the optimistic line to that id, mark it
    // sent, cache it, and advance the cursor. Either way (a 0-id ack means the
    // row was not durable -- a tagma-side anomaly), clear the in-flight slot so
    // the pump does not wedge, then pump the next queued message.
    if (reply.kind === "message_accepted" && ch.pendingLocalId !== null) {
      const ackId = reply.history_id ?? 0;
      if (ackId > 0) {
        ch.transcript = replaceLineId(ch.transcript, ch.pendingLocalId, ackId);
        const confirmed = ch.transcript.lines.find(
          (l) => l.historyId === ackId,
        );
        if (confirmed) {
          void cachePut({
            conversationId,
            historyId: ackId,
            role: "user",
            text: confirmed.text,
          });
        }
        if (ackId > ch.maxRendered) ch.maxRendered = ackId;
      }
      ch.pendingLocalId = null;
      void this.pumpPending(ch);
    }
    if (reply.kind === "history_batch_end") {
      // Normal catch-up completion: cancel the open() watchdog (no longer
      // needed) and flip live so subsequent frames may notify.
      if (ch.liveWatchdog) {
        clearTimeout(ch.liveWatchdog);
        ch.liveWatchdog = null;
      }
      ch.live = true;
    }
    if (ch.live) maybeNotifyBackground(ch.label, reply);
  }
}

export const channelsStore = new ChannelsStore();
