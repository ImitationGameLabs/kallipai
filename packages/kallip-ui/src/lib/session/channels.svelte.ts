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

import {
  type Envelope,
  openRelayChannel,
  type RelayChannel,
  type TagmaReply,
  type TagmaView,
} from "@kallipai/kallip-agora-client";
import { SvelteMap } from "svelte/reactivity";
import {
  agoraClientOrFail,
  agoraSession,
  lescheClientOrFail,
} from "./agora.svelte.ts";
import {
  applyTagmaReply,
  type ChannelTranscript,
  EMPTY_TRANSCRIPT,
  withUserLine,
} from "../channel/transcript.ts";
import type { NavIndicator } from "../shell.ts";

function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** Build a synthetic `error` reply from a thrown exception, so a send/interrupt
 * failure routes through the same reducer as a herald-side error. `req_id` and
 * `status` are sentinels (the failure did not originate from a herald reply). */
function syntheticErrorReply(message: string): TagmaReply {
  return { kind: "error", req_id: 0, status: 0, message };
}

/** Map a channel's transport status to a sidebar dot. This is OUR channel
 * transport (the KEX/drain lifecycle), distinct from the dashboard's
 * `TagmaPresence` / `realtimeStore` presence, which is the PEER presence (a
 * herald tunnel is live). `error` (KEX/drain failure) is kept distinct from
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
  pending: string[] = $state([]);
  status: "opening" | "open" | "offline" | "error" = $state("opening");
  // The transport; plain (not reactive). Set once openRelayChannel resolves.
  channel: RelayChannel | null = null;

  constructor(
    readonly convId: string,
    readonly tagmaId: string,
    readonly label: string | null,
  ) {}
}

class ChannelsStore {
  /** convId -> channel state. `SvelteMap` (not `$state(new Map())`): Svelte's
   * `$state` proxy does not wrap Map/Set, so a raw Map's in-place `.set()`
   * would be invisible to reactivity and the sidebar would never update.
   * SvelteMap tracks membership + iteration natively; per-field reactivity of
   * each entry still comes from ChannelState's own runes. */
  private channels = new SvelteMap<string, ChannelState>();

  /** Snapshot for the sidebar: convId/label/indicator per open channel. Reading
   * `c.status` here also subscribes the indicator to mid-life status changes
   * (e.g. drain flipping open -> offline). */
  get list(): {
    convId: string;
    label: string | null;
    indicator: NavIndicator;
  }[] {
    return Array.from(this.channels.values()).map((c) => ({
      convId: c.convId,
      label: c.label,
      indicator: channelIndicator(c.status),
    }));
  }

  get(convId: string): ChannelState | undefined {
    return this.channels.get(convId);
  }

  /**
   * Open an E2EE channel to `tagma`. Runs the key exchange, then drains the
   * channel's reply stream into the transcript. Resolves to the conversation id
   * (for navigation). Throws if the user is unsigned or KEX fails. The caller
   * (the card, gated on realtime presence) ensures the tagma is online; a stale
   * click surfaces the KEX failure inline.
   */
  async open(tagma: TagmaView): Promise<string> {
    const userId = agoraSession.user?.user_id;
    if (!userId) throw new Error("not signed in");

    // KEX is synchronous HTTP; inbound replies flow only through realtime's SSE
    // demux into `deliver`, which begins well before the user can send.
    const channel = await openRelayChannel(
      agoraClientOrFail(),
      lescheClientOrFail(),
      tagma.tagma_id,
      userId,
    );
    const state = new ChannelState(channel.convId, tagma.tagma_id, tagma.label);
    state.channel = channel;
    state.status = "open";
    this.channels.set(channel.convId, state);
    void this.drain(channel.convId);
    return channel.convId;
  }

  /** Send a prompt, or queue it if the agent is mid-turn. */
  async send(convId: string, text: string): Promise<void> {
    const ch = this.channels.get(convId);
    const trimmed = text.trim();
    if (!ch || !ch.channel || trimmed === "") return;
    if (ch.transcript.status === "busy") {
      ch.pending = [...ch.pending, trimmed];
      return;
    }
    ch.transcript = withUserLine(ch.transcript, trimmed);
    try {
      await ch.channel.send(trimmed);
    } catch (e) {
      ch.transcript = applyTagmaReply(
        ch.transcript,
        syntheticErrorReply(messageOf(e)),
      );
    }
  }

  /** Interrupt the in-flight turn. */
  async interrupt(convId: string): Promise<void> {
    const ch = this.channels.get(convId);
    if (!ch?.channel) return;
    try {
      await ch.channel.interrupt();
    } catch (e) {
      ch.transcript = applyTagmaReply(
        ch.transcript,
        syntheticErrorReply(messageOf(e)),
      );
    }
  }

  /** Close + drop a channel. */
  close(convId: string): void {
    const ch = this.channels.get(convId);
    ch?.channel?.close();
    this.channels.delete(convId);
  }

  /** Route an inbound envelope (handed off by realtime.svelte.ts's SSE demux)
   * to the channel that owns its conversation. Unknown ids are dropped -- the
   * envelope belongs to a channel the app has not opened. */
  deliver(envelope: Envelope): void {
    this.channels.get(envelope.conversation_id)?.channel?.enqueue(envelope);
  }

  /** Tear down every open channel. Called by the shell on logout and on leaving
   * online mode. The SSE subscriber is owned by realtime.svelte.ts and torn down
   * separately; this only closes the per-channel transports + clears the map. */
  reset(): void {
    for (const ch of this.channels.values()) ch.channel?.close();
    this.channels.clear();
  }

  // --- internals -----------------------------------------------------------

  /** Drain a channel's reply stream into its transcript. Ends when the channel
   * is closed (replies generator ends); the channel then reads as offline. */
  private async drain(convId: string): Promise<void> {
    const ch = this.channels.get(convId);
    if (!ch?.channel) return;
    const channel = ch.channel;
    try {
      for await (const reply of channel.replies()) {
        ch.transcript = applyTagmaReply(ch.transcript, reply);
        if (ch.transcript.status === "idle") await this.flushPending(convId);
      }
    } catch {
      if (this.channels.get(convId) === ch) ch.status = "error";
    } finally {
      if (this.channels.get(convId) === ch && ch.status === "open") {
        ch.status = "offline";
      }
    }
  }

  private async flushPending(convId: string): Promise<void> {
    const ch = this.channels.get(convId);
    if (!ch?.channel || ch.pending.length === 0) return;
    const text = ch.pending.join("\n");
    ch.pending = [];
    ch.transcript = withUserLine(ch.transcript, text);
    try {
      await ch.channel.send(text);
    } catch (e) {
      ch.transcript = applyTagmaReply(
        ch.transcript,
        syntheticErrorReply(messageOf(e)),
      );
    }
  }
}

export const channelsStore = new ChannelsStore();
