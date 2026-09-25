// The lesche direct-session store: the console's window onto the agent-to-
// agent 1v1 conversations its enrolled tagmas keep on the relay. Per enrolled
// tagma with an open relay channel, the store polls the daemon's aggregated
// session list (`GET /agents/{id}/lesche/sessions` over the channel's manage
// bridge), keeps the `direct` rows, and dedups the two daemons' views of the
// same pair (A↔B appears in both lists) to the canonical min-side daemon --
// the same ordering the lesche's own pair derivation uses.
//
// Read-only in v1: transcripts are pulled by the conversation page through
// {@link directSessionsStore.fetchTranscript}; there is no push path (no
// read-cursor route, no lesche→browser stream), so rows refresh on the poll
// tick only and the hub/sidebar carry no unread badges.
//
// Naming note: "direct session" here is the conversation KIND (two tagmas on
// the relay); the existing `DirectTransport` is the offline direct-connection
// transport modality. Same adjective, different axis.

import { SvelteMap } from "svelte/reactivity";
import type {
  DirectMessageRow,
  LescheSessionEntry,
} from "@kallipai/kallipai-client";
import { archeionSession } from "./archeion.svelte";
import { channelsStore } from "./channels.svelte";
import { chat_direct_fallback } from "../../paraglide/messages.js";

/** One deduped direct session as the console lists it: the pair (two of this
 * owner's tagmas) plus the daemon to fetch through (the canonical min side
 * when enrolled, else whichever side reported the row) and the peer's display
 * facts. */
export interface DirectSessionEntry {
  /** The daemon the transcript is fetched through. */
  readonly tagmaId: string;
  /** The other member of the pair. */
  readonly peerId: string;
  /** The peer's relay-stamped handle; "" when the relay had none. */
  readonly peerHandle: string;
  /** The derived session id (for logs/keys; never user-facing). */
  readonly sessionId: string;
}

/** Poll cadence for the aggregated session list: slow enough to be invisible,
 * fast enough that a session created on the peer daemon self-heals within a
 * tick or two (the cold-window behavior the rooms roster already has). */
const POLL_MS = 30_000;

/** A pair unseen for this many consecutive ticks is pruned (the session was
 * removed, or its daemon was revoked). Generous enough that a few failed
 * polls (relay restart, dead channel mid-rekey) do not flash rows away. */
const PRUNE_TICKS = 5;

export class DirectSessionsStore {
  /** pairKey (min|max tagma ids) → entry. */
  #entries = new SvelteMap<string, DirectSessionEntry>();
  /** pairKey → last tick that reported the pair (prune stamp). */
  #seen = new Map<string, number>();
  /** tagmaId → root agent id (stable per daemon; resolved over the manage
   * bridge once, because `/agents/root` is an HTTP special case that is not
   * in the manage subset). */
  #rootIds = new Map<string, string>();
  #tickNo = 0;
  #timer: ReturnType<typeof setInterval> | null = null;
  #inFlight = new Set<string>();

  /** The deduped rows, sorted by display label. */
  list(): DirectSessionEntry[] {
    return [...this.#entries.values()].sort((a, b) =>
      this.peerLabel(a.peerId, a.peerHandle).localeCompare(
        this.peerLabel(b.peerId, b.peerHandle),
      ),
    );
  }

  /** Best display label for a peer: the enrollment's label (the owner's own
   * name for the tagma), else the relay-stamped handle, else a short id. */
  peerLabel(peerId: string, peerHandle?: string): string {
    const enrolled = archeionSession.enrolledCards.find(
      (t) => t.tagmaId === peerId,
    );
    if (enrolled?.label) return enrolled.label;
    if (peerHandle) return peerHandle;
    return chat_direct_fallback({ id: peerId.slice(0, 8) });
  }

  /** Start polling (signed-in online). Idempotent. */
  start(): void {
    if (this.#timer !== null) return;
    void this.tick();
    this.#timer = setInterval(() => void this.tick(), POLL_MS);
  }

  stop(): void {
    if (this.#timer !== null) {
      clearInterval(this.#timer);
      this.#timer = null;
    }
  }

  /** One poll sweep over every enrolled tagma, then prune. Background tabs
   * skip the sweep (the store is a convenience, not a sync engine). Public
   * for tests: production only calls it from start()'s interval. */
  async tick(): Promise<void> {
    if (typeof document !== "undefined" && document.hidden) return;
    this.#tickNo += 1;
    const tick = this.#tickNo;
    await Promise.allSettled(
      archeionSession.enrolledCards.map((t) =>
        this.refreshTagma(t.tagmaId, tick),
      ),
    );
    for (const [key, entry] of this.#entries) {
      const fresh = (this.#seen.get(key) ?? 0) >= tick - PRUNE_TICKS + 1;
      if (!fresh && entry.peerId !== "") this.#entries.delete(key);
    }
  }

  /** Fetch one daemon's direct rows through its open relay channel and merge.
   * Best-effort: a tagma with no open channel (KEX pending, offline, revoked)
   * contributes nothing this tick. Public for tests. */
  async refreshTagma(tagmaId: string, tick = this.#tickNo): Promise<void> {
    if (this.#inFlight.has(tagmaId)) return;
    this.#inFlight.add(tagmaId);
    try {
      const rows = await this.fetchDirectRows(tagmaId);
      for (const row of rows) {
        this.#mergeRow(row, tick);
      }
    } catch {
      // Best-effort: the sweep retries next tick.
    } finally {
      this.#inFlight.delete(tagmaId);
    }
  }

  /** Pull one direct session's transcript page as typed rows (the
   * `format=json` variant). Backs the conversation page's hydrate and its
   * tail poll: `afterSeq` is exclusive, rows ascend. Bounded 502 downgrade:
   * the manage bridge drops a response over its 256 KiB cap WHOLE, so a
   * too-big page is retried at half the limit down to a floor of 1 -- never
   * the same params twice. */
  async fetchTranscript(
    tagmaId: string,
    peerId: string,
    afterSeq: number,
    limit = 25,
  ): Promise<DirectMessageRow[]> {
    let attempt = limit;
    for (;;) {
      const root = await this.rootId(tagmaId);
      const res = await this.manage(
        tagmaId,
        "GET",
        `/agents/${root}/lesche/direct-sessions/${peerId}/messages` +
          `?format=json&after_seq=${afterSeq}&limit=${attempt}`,
      );
      if (res.status !== 502) {
        if (res.status !== 200) {
          throw new Error(`direct history fetch failed: ${res.status}`);
        }
        return res.body as DirectMessageRow[];
      }
      if (attempt <= 1) {
        throw new Error(
          "direct history unavailable: 502 held at the floor limit",
        );
      }
      attempt = Math.max(1, Math.floor(attempt / 2));
    }
  }

  /** The plumbing leg: resolve the tagma's open channel and run a manage op.
   * Throws when the tagma has no open relay conversation. */
  protected manage(
    tagmaId: string,
    method: string,
    path: string,
  ): Promise<{ status: number; body: unknown }> {
    const channel = channelsStore.relayChannelOf(tagmaId);
    if (!channel) {
      throw new Error(`no open channel for ${tagmaId}`);
    }
    return channel.manage(method, path, null);
  }

  /** The root agent id for a daemon, resolved once (the manage subset has no
   * `/agents/root` special case; the root is the one agent `created_by`
   * nothing -- the registry's documented root invariant). */
  protected async rootId(tagmaId: string): Promise<string> {
    const cached = this.#rootIds.get(tagmaId);
    if (cached) return cached;
    const res = await this.manage(tagmaId, "GET", "/agents");
    if (res.status !== 200) {
      throw new Error(`agent list failed: ${res.status}`);
    }
    const root = (
      res.body as Array<{ id: string; created_by: string | null }>
    ).find((a) => a.created_by === null);
    if (!root) throw new Error(`no root agent on ${tagmaId}`);
    this.#rootIds.set(tagmaId, root.id);
    return root.id;
  }

  /** The overridable fetch leg (tests script this): one daemon's direct rows
   * as entries keyed to this daemon. Production derives it from the daemon's
   * aggregated session list over the manage bridge. */
  protected async fetchDirectRows(
    tagmaId: string,
  ): Promise<DirectSessionEntry[]> {
    const root = await this.rootId(tagmaId);
    const res = await this.manage(
      tagmaId,
      "GET",
      `/agents/${root}/lesche/sessions`,
    );
    if (res.status !== 200) {
      throw new Error(`session list failed: ${res.status}`);
    }
    return (res.body as LescheSessionEntry[])
      .filter((e) => e.kind === "direct")
      .map((e) => ({
        tagmaId,
        peerId: e.peer_tagma ?? "",
        peerHandle: e.peer_handle ?? "",
        sessionId: e.id,
      }))
      .filter((e) => e.peerId !== "");
  }

  /** Canonical-min dedup: a pair reported by both daemons keeps the min-side
   * row (its daemon is the fetch-through home); a row only the max side sees
   * (min side revoked, or not yet reporting) is kept until the min side
   * reports or the prune sweep ages it out. */
  #mergeRow(row: DirectSessionEntry, tick: number): void {
    const key = pairKey(row.tagmaId, row.peerId);
    this.#seen.set(key, tick);
    const existing = this.#entries.get(key);
    if (!existing) {
      this.#entries.set(key, row);
      return;
    }
    const incomingIsMin = row.tagmaId < row.peerId;
    const existingIsMin = existing.tagmaId < existing.peerId;
    if (incomingIsMin && !existingIsMin) this.#entries.set(key, row);
  }

  /** Test seam: merge scripted rows exactly as a fetch would. */
  mergeForTest(rows: DirectSessionEntry[], tick = this.#tickNo): void {
    for (const row of rows) this.#mergeRow(row, tick);
  }
}

function pairKey(a: string, b: string): string {
  return a < b ? `${a}|${b}` : `${b}|${a}`;
}

export const directSessionsStore = new DirectSessionsStore();
