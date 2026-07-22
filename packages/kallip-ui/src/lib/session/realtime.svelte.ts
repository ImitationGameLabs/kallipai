// RealtimeStore: the online-mode realtime feed. Owns the single multiplexed
// SSE subscription to the lesche's `GET /v1/me/events` and demuxes its two
// concerns: tagma presence (`tagma_online` / `tagma_offline`, the SOLE liveness
// signal -- the agora's `/v1/tagmata` no longer carries an `online` field) and
// inbound conversation `envelope` delivery (handed to channelsStore via a
// shell-wired sink).
//
// The SSE loop (backoff, AgoraApiError 401 stop, abort) was moved here verbatim
// from channels.svelte.ts, which is now pure per-channel chat state. Realtime is
// started/stopped by RootLayout (reactive to online mode + a signed-in user);
// it must run before any channel opens, because the /tagmata dashboard reads
// presence to light the online dot + gate the "Open channel" button.
//
// Dependency direction is one-way: realtime -> agora (the lesche client port).
// It does NOT import channels -- the envelope sink is bound by the shell
// (RootLayout), keeping the two stores decoupled.

import {
  AgoraApiError,
  type AgoraEvent,
  type Envelope,
} from "@kallipai/kallip-agora-client";
import { SvelteSet } from "svelte/reactivity";
import { lescheClientOrFail } from "./agora.svelte.ts";

/** Sink for inbound conversation envelopes. Bound by the shell to
 * `channelsStore.deliver`. `null` (the default) drops envelopes -- harmless
 * before the shell wires it, since no channel can be open yet. */
type EnvelopeSink = (envelope: Envelope) => void;

/** Maximum time the dashboard shows the "checking" placeholder before treating
 * presence as resolved (unknown tagmas then read offline). Bounded so a missing
 * or churning SSE connection can never strand the UI in "checking" forever --
 * the first presence event resolves presence immediately, this is only the
 * backstop for the no-event case (e.g. herald never started -> empty snapshot,
 * or the SSE can't connect). A per-connection grace timer (the previous design)
 * was deliberately NOT re-added: on a churning connection it resets every
 * reconnect and never fires, which is the exact bug this deadline replaces. */
const RESOLVE_DEADLINE_MS = 2000;

class RealtimeStore {
  // Online tagma ids -- the PEER presence (a herald tunnel is live for this
  // tagma), shown by the /tagmata dashboard dot. `SvelteSet` (not `$state(new
  // Set())`): Svelte's `$state` proxy does not wrap Set, so a raw Set's in-place
  // `.add()/.delete()` would be invisible to reactivity. SvelteSet tracks
  // membership natively. Distinct from `ChannelState.status` (OUR channel
  // transport), shown by the sidebar dot via channels.svelte.ts `channelIndicator`.
  private presence = new SvelteSet<string>();
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
  // One-shot per session; force-resolves presence after the deadline so the
  // "checking" placeholder is bounded regardless of SSE connection health.
  private resolveDeadline: ReturnType<typeof setTimeout> | null = null;

  /** Reactive liveness query: true iff a herald tunnel is live for `tagmaId`. */
  has(tagmaId: string): boolean {
    return this.presence.has(tagmaId);
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

  /** Start the SSE subscriber, idempotently. Safe to call repeatedly. Clears
   * presence once per session so a stale set from a prior session cannot leak;
   * the lesche's connect-time snapshot then repopulates it. Arms the one-shot
   * resolve deadline so "checking" is bounded even if the SSE never connects. */
  start(): void {
    if (this.running) return;
    this.running = true;
    this.clearResolveDeadline();
    this.presence.clear();
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
   * the next session reset; this is rare (the herald tunnel is the only offline
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
        if (e instanceof AgoraApiError && e.status === 401) {
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

  private dispatch(ev: AgoraEvent): void {
    switch (ev.type) {
      case "tagma_online":
        this.presence.add(ev.tagma_id);
        break;
      case "tagma_offline":
        this.presence.delete(ev.tagma_id);
        break;
      case "envelope":
        this.envelopeSink?.(ev.envelope);
        break;
      case "agent_state":
        // Reserved; the lesche does not emit it yet.
        break;
    }
  }
}

export const realtimeStore = new RealtimeStore();
