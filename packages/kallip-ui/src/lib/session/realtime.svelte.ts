// RealtimeStore: the online-mode realtime feed. Owns the single multiplexed
// SSE subscription to the lesche's `GET /v1/me/events` and demuxes its two
// concerns: tagma presence (`tagma_online` / `tagma_offline`, the SOLE liveness
// signal -- the agora's `/v1/tagmata` no longer carries an `online` field) and
// inbound conversation `envelope` delivery (handed to channelsStore via a
// shell-wired sink).
//
// The SSE loop (backoff, LescheApiError 401 stop, abort) was moved here verbatim
// from channels.svelte.ts, which is now pure per-channel chat state. Realtime is
// started/stopped by RootLayout (reactive to online mode + a signed-in user);
// it must run before any channel opens, because the /tagmata dashboard reads
// presence to light the online dot and the presence sink drives auto-connect
// (the shell opens a channel on an offline -> online transition).
//
// Dependency direction is one-way: realtime -> agora (the lesche client port).
// It does NOT import channels -- the envelope sink is bound by the shell
// (RootLayout), keeping the two stores decoupled.

import {
  LescheApiError,
  type LescheEvent,
  type Envelope,
  type SignalEvent,
} from "@kallipai/kallip-lesche-client";
import { SvelteMap, SvelteSet } from "svelte/reactivity";
import type { TagmaStatusSummary } from "../tagmata.svelte.ts";
import { lescheClientOrFail } from "./agora.svelte.ts";

/** Sink for inbound conversation envelopes. Bound by the shell to
 * `channelsStore.deliver`. `null` (the default) drops envelopes -- harmless
 * before the shell wires it, since no channel can be open yet. */
type EnvelopeSink = (envelope: Envelope) => void;

/** Sink for tagma presence transitions. Bound by the shell; the online
 * transition drives channelsStore auto-connect (it opens a channel to that
 * tagma). The offline transition is currently a no-op in the shell (a channel
 * detects peer-offline via its own drain), but the bidirectional signature
 * keeps the door open for future peer-offline eviction. Fired only on actual
 * transitions (not on every reconnect snapshot re-send), so a healthy session
 * is not flooded with no-op opens. */
type PresenceSink = (tagmaId: string, online: boolean) => void;

/** Sink for inbound runtime signals (busy/idle presence, turn terminals,
 * errors). Bound by the shell to `channelsStore.deliverSignal`. `null` (the
 * default) drops signals -- harmless before the shell wires it. */
type SignalSink = (tagmaId: string, signal: SignalEvent) => void;

/** Sink for aggregate status snapshots. Bound by the shell to
 * `channelsStore.deliverStatus`, which routes each snapshot to the open
 * relay conversation for that tagma so its chat header has a uniform status
 * source (mirroring how the direct path drains status off its own SSE).
 * `undefined` evicts (a tagma went offline). `null` (the default) drops -- the
 * /tagmata dashboard still reads `statusFor` directly off this store's map. */
type StatusSink = (
  tagmaId: string,
  snapshot: TagmaStatusSummary | undefined,
) => void;

/** Sink for `room_membership_changed` nudges (a member was added/removed). Bound
 * by the shell to `roomConversationsStore.refreshRoster` so a roster change
 * repaints without waiting for the room page's poll. `null` (the default)
 * drops. */
type RoomMembershipChangedSink = (roomId: string) => void;

/** Sink for `room_member_online` / `room_member_offline` deltas. Bound by the
 * shell to `roomConversationsStore.applyMemberPresence` so a peer's room
 * presence transition mutates that room's online-member set live, between
 * roster re-fetches. `null` (the default) drops. */
type RoomMemberPresenceSink = (
  roomId: string,
  memberId: string,
  online: boolean,
) => void;

/** Maximum time the dashboard shows the "checking" placeholder before treating
 * presence as resolved (unknown tagmas then read offline). Bounded so a missing
 * or churning SSE connection can never strand the UI in "checking" forever --
 * the first presence event resolves presence immediately, this is only the
 * backstop for the no-event case (e.g. tagma never started -> empty snapshot,
 * or the SSE can't connect). A per-connection grace timer (the previous design)
 * was deliberately NOT re-added: on a churning connection it resets every
 * reconnect and never fires, which is the exact bug this deadline replaces. */
const RESOLVE_DEADLINE_MS = 2000;

class RealtimeStore {
  // Online tagma ids -- the PEER presence (a tagma tunnel is live for this
  // tagma), shown by the /tagmata dashboard dot. `SvelteSet` (not `$state(new
  // Set())`): Svelte's `$state` proxy does not wrap Set, so a raw Set's in-place
  // `.add()/.delete()` would be invisible to reactivity. SvelteSet tracks
  // membership natively. Distinct from `ChannelState.status` (OUR channel
  // transport), shown by the sidebar dot via links.ts `tagmaNavIndicator`
  // (which reads `channelsStore.getTagmaChannelState`).
  private presence = new SvelteSet<string>();
  // Per-tagma aggregate status snapshots, fed by the `tagma_status` SSE event.
  // `SvelteMap` (not `$state(new Map())`): Svelte's `$state` proxy does not
  // wrap Map, so a raw Map's in-place `.set()` would be invisible to
  // reactivity. SvelteMap tracks entries natively. Consumed by both the
  // /tagmata dashboard cards and the channel-chat status header via
  // `statusFor` -- one source of truth for both surfaces.
  private status = new SvelteMap<string, TagmaStatusSummary>();
  // False until presence has been resolved for this session -- either the first
  // presence event arrives, or the one-shot `resolveDeadline` (armed in `start`)
  // fires. The dashboard shows a "checking" placeholder only while this is
  // false; once true, unknown tagmas read offline (not "checking"). Stays true
  // across SSE reconnects within a session -- re-arming "checking" mid-session
  // would reintroduce the flap the no-clear-on-reconnect policy avoids.
  private resolvedState = $state(false);
  private running = false;
  private abort: AbortController | null = null;
  private envelopeSink: EnvelopeSink | null = null;
  private presenceSink: PresenceSink | null = null;
  private signalSink: SignalSink | null = null;
  private statusSink: StatusSink | null = null;
  private roomMembershipChangedSink: RoomMembershipChangedSink | null = null;
  private roomMemberPresenceSink: RoomMemberPresenceSink | null = null;
  // One-shot per session; force-resolves presence after the deadline so the
  // "checking" placeholder is bounded regardless of SSE connection health.
  private resolveDeadline: ReturnType<typeof setTimeout> | null = null;

  /** Reactive liveness query: true iff a tagma tunnel is live for `tagmaId`. */
  has(tagmaId: string): boolean {
    return this.presence.has(tagmaId);
  }

  /** Reactive status query: the latest aggregate snapshot for `tagmaId`, or
   * `undefined` while none has arrived (freshly connected, or offline tagma).
   * Both the dashboard card and the channel-chat header read from here. */
  statusFor(tagmaId: string): TagmaStatusSummary | undefined {
    return this.status.get(tagmaId);
  }

  /** True once presence has been resolved for this session -- either the first
   * presence event arrived, or the `RESOLVE_DEADLINE_MS` backstop elapsed. Until
   * then the dashboard shows a "checking" placeholder; once true, unknown
   * tagmas read offline (the safe default), not "checking". */
  get resolved(): boolean {
    return this.resolvedState;
  }

  /** Bind the inbound-envelope handler. Called once by the shell at boot. */
  setEnvelopeSink(sink: EnvelopeSink | null): void {
    this.envelopeSink = sink;
  }

  /** Bind the presence-transition handler. Called once by the shell at boot;
   * drives channelsStore auto-connect on offline -> online transitions. */
  setPresenceSink(sink: PresenceSink | null): void {
    this.presenceSink = sink;
  }

  /** Bind the inbound-signal handler. Called once by the shell at boot; routes
   * `tagma_signal` events (busy/idle, terminals, errors) into channelsStore. */
  setSignalSink(sink: SignalSink | null): void {
    this.signalSink = sink;
  }

  /** Bind the status-snapshot handler. Called once by the shell at boot; routes
   * `tagma_status` snapshots (and offline evictions) into channelsStore so the
   * owning relay conversation's chat header has a uniform status source. */
  setStatusSink(sink: StatusSink | null): void {
    this.statusSink = sink;
  }

  /** Bind the room-membership-changed handler. Called once by the shell at boot;
   * routes `room_membership_changed` nudges into `roomConversationsStore
   * .refreshRoster` so a roster change repaints without waiting for the poll. */
  setRoomMembershipChangedSink(sink: RoomMembershipChangedSink | null): void {
    this.roomMembershipChangedSink = sink;
  }

  /** Bind the room-member-presence handler. Called once by the shell at boot;
   * routes `room_member_online`/`room_member_offline` deltas into
   * `roomConversationsStore.applyMemberPresence` so a peer's transition mutates
   * that room's live online-member set. */
  setRoomMemberPresenceSink(sink: RoomMemberPresenceSink | null): void {
    this.roomMemberPresenceSink = sink;
  }

  /** Start the SSE subscriber, idempotently. Safe to call repeatedly. Clears
   * presence once per session so a stale set from a prior session cannot leak;
   * the lesche's connect-time snapshot then repopulates it. Arms the one-shot
   * resolve deadline so "checking" is bounded even if the SSE never connects. */
  start(): void {
    if (this.running) return;
    this.running = true;
    this.clearResolveDeadline();
    this.presence.clear();
    this.status.clear();
    this.resolvedState = false;
    this.resolveDeadline = setTimeout(
      () => this.markResolved(),
      RESOLVE_DEADLINE_MS,
    );
    this.abort = new AbortController();
    void this.run();
  }

  /** Stop the subscriber, abort the in-flight fetch, and drop presence. Called
   * by the shell when leaving online mode or on logout (the cookie is gone, so
   * presence is meaningless until re-auth). */
  stop(): void {
    this.running = false;
    this.abort?.abort();
    this.abort = null;
    this.clearResolveDeadline();
    this.presence.clear();
    this.status.clear();
    this.resolvedState = false;
  }

  private clearResolveDeadline(): void {
    if (this.resolveDeadline !== null) {
      clearTimeout(this.resolveDeadline);
      this.resolveDeadline = null;
    }
  }

  /** Mark presence as resolved for this session (first event or deadline). */
  private markResolved(): void {
    this.clearResolveDeadline();
    this.resolvedState = true;
  }

  /** The reconnect loop. Presence is intentionally NOT cleared on reconnect:
   * doing so would flash every connected tagma offline until the snapshot
   * repaints it, which (with an idle-prone SSE connection) shows up as the
   * online/offline dot flapping. Instead the lesche re-sends the presence
   * snapshot on every connect, so a reconnect idempotently re-adds the
   * still-online set with no flicker. The trade-off is that a tagma whose
   * `tagma_offline` was missed during a disconnect can read stale-online until
   * the next session reset; this is rare (the tagma tunnel is the only offline
   * source and it is stable) and self-corrects on a refresh. */
  private async run(): Promise<void> {
    let backoff = 1000;
    const signal = this.abort!.signal;
    while (this.running) {
      try {
        for await (const ev of lescheClientOrFail().meEvents(signal)) {
          backoff = 1000; // a live event proves the stream is healthy.
          // The first event (incl. the connect-time presence snapshot) resolves
          // presence immediately. The no-event case (empty snapshot, or the SSE
          // never connects) is handled by the one-shot deadline armed in start.
          this.markResolved();
          this.dispatch(ev);
        }
        // Stream ended cleanly (rare); loop to reconnect unless stopped.
      } catch (e) {
        // A 401 means the session is gone -- stop rather than hot-looping
        // reconnects against an unsigned user.
        if (e instanceof LescheApiError && e.status === 401) {
          this.running = false;
          this.clearResolveDeadline();
          this.presence.clear();
          this.resolvedState = false;
          return;
        }
        // Other errors (transient network, server drop): reconnect after backoff.
      }
      if (!this.running) break;
      await new Promise((r) => setTimeout(r, backoff));
      backoff = Math.min(backoff * 2, 30_000);
    }
  }

  private dispatch(ev: LescheEvent): void {
    switch (ev.type) {
      case "tagma_online":
        // Transition only: the lesche re-sends the presence snapshot on every
        // SSE reconnect, and the no-clear-on-reconnect policy (see `run`)
        // leaves the set populated, so a re-send for an already-online tagma
        // is a no-op here. Firing the sink only on a real transition keeps a
        // healthy reconnect from flooding channelsStore with redundant opens.
        if (this.presence.has(ev.tagma_id)) break;
        this.presence.add(ev.tagma_id);
        this.presenceSink?.(ev.tagma_id, true);
        break;
      case "tagma_offline":
        if (!this.presence.has(ev.tagma_id)) break;
        this.presence.delete(ev.tagma_id);
        // Evict the last status snapshot so an offline tagma does not keep
        // rendering stale agent counts/budget -- `statusFor` returns undefined
        // (the card hides its line, the header shows "waiting…") until the
        // tagma reconnects and a fresh snapshot arrives.
        this.status.delete(ev.tagma_id);
        this.statusSink?.(ev.tagma_id, undefined);
        this.presenceSink?.(ev.tagma_id, false);
        break;
      case "envelope":
        this.envelopeSink?.(ev.envelope);
        break;
      case "tagma_status":
        // Map the snake_case wire fields to the camelCase internal shape at
        // this single dispatch boundary, so consumers read idiomatic TS.
        this.status.set(ev.tagma_id, {
          rootState: ev.root_state,
          subagentsTotal: ev.subagents_total,
          subagentsActive: ev.subagents_active,
          tokenBudget: ev.token_budget,
          tokenConsumed: ev.token_consumed,
        });
        this.statusSink?.(ev.tagma_id, this.status.get(ev.tagma_id));
        break;
      case "tagma_signal":
        // A per-event runtime signal (busy/idle presence, turn terminals,
        // errors). Plaintext operator metadata, not conversation content;
        // routed to the owning channel's transcript as a status transition
        // and/or a transient system line.
        this.signalSink?.(ev.tagma_id, ev.event);
        break;
      case "room_membership_changed":
        // A room's membership epoch bumped (a member was added/removed). The
        // user-device analog of the tagma `Wake`: a transient nudge to refresh
        // the room roster (membership is server-authoritative). NOT buffered --
        // a dropped frame is backstopped by the room page's poll, not replay.
        this.roomMembershipChangedSink?.(ev.room_id);
        break;
      case "room_member_online":
        // A peer came online in this room (agent tunnel / human app stream).
        // Idempotent set-add on the owning room's online set (see
        // roomConversationsStore.applyMemberPresence); the roster re-fetch
        // remains the resync ground truth.
        this.roomMemberPresenceSink?.(ev.room_id, ev.member_id, true);
        break;
      case "room_member_offline":
        // A peer went offline. Idempotent set-remove; transient (not buffered),
        // so a missed frame self-heals on the next roster re-fetch.
        this.roomMemberPresenceSink?.(ev.room_id, ev.member_id, false);
        break;
      default: {
        // Exhaustiveness guard: a new LescheEvent variant without a dispatch
        // case fails the build here rather than silently dropping.
        const _exhaustive: never = ev;
        break;
      }
    }
  }
}

export const realtimeStore = new RealtimeStore();
