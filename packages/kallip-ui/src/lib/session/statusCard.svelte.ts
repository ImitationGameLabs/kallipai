// Status-card rows: the per-agent list under the chat status bar. Roster
// refresh is event-nudged first -- the chat page routes every status-snapshot
// update (relay `tagma_status` push online, direct SSE drain offline) into
// nudge() -- with a slow reconciliation poll as the dropped-frame backstop:
// the Online (projection-feed) path runs roster and context polls at 30s
// alongside the dirty SSE (a half-open stream looks alive, so the poll is
// the only thing that notices it died); the Offline path polls at 15s/30s.
// Both cadences pause while the tab is hidden, over whichever
// ManagementBackend the conversation provides (OnlineBackend on the relay
// channel, OfflineBackend direct). Context occupancy mirrors the detail
// page's approximation (turn tokens + pinned) with the registry profile's
// max window as denominator; faulted/parked agents get no context column --
// their status response is a 409 or meaningless.

import type { AgentState, WireParkedReason } from "@kallipai/kallip-client";
import { type ManagementBackend } from "../manage/backend.ts";
import { startVisibleInterval } from "../visibleInterval.ts";
import type { TagmaStatusSummary } from "../tagmata.svelte.ts";

/** One rendered row. `contextTokens` is null until the slow poll lands (or
 * forever, for faulted/parked agents). */
/** Failed context pulls retry at most once per this window; the 30s
 * poll remains the unconditional backstop. */
const CONTEXT_RETRY_COOLDOWN_MS = 30_000;

/** The Online (projection-feed) backstop cadence: the interval that keeps
 * polling when the dirty SSE delivers nothing -- either because nothing
 * changed or because the stream died silently. Both legs (roster and
 * contexts) run at this cadence, matching the Offline path's slow leg. */
const BACKSTOP_INTERVAL_MS = 30_000;
export interface StatusCardRow {
  readonly id: string;
  readonly state: AgentState;
  readonly role: string;
  readonly activity: string;
  /** Wire's free-form words for the agent; drives the row hover tooltip.
   * Empty when the wire does not set one. */
  readonly description: string;
  readonly contextTokens: number | null;
  /** Registry max window for the row's profile; null until the
   * attach-time registry pull lands (or the profile is unknown). */
  readonly contextWindow: number | null;
  /** Present only for parked rows; appended to the row's hover tooltip. */
  readonly parkedReason: WireParkedReason | null;
}

// Attention order: work states rise, terminal anomalies stay visible above
// idle (a fault must be seen), idle sinks into the
// fold. Keys are state + id only, never activity text, so a poll refresh
// re-renders in place instead of reshuffling rows.
const STATE_ORDER: Record<AgentState, number> = {
  busy: 0,
  waiting: 1,
  retrying: 2,
  faulted: 3,
  parked: 4,
  idle: 5,
};

class StatusCardStore {
  rootRow = $state<StatusCardRow | null>(null);
  subRows = $state<readonly StatusCardRow[]>([]);
  // Aggregate mirror: the latest TagmaStatusSummary the transports pushed
  // (the relay SSE via the shell's status sink, the direct drain via the
  // conversation). Undefined until the first push; kept across suspend()
  // like the rows, so a drawer opened on any route reads the last known
  // truth instead of a waiting placeholder.
  summary = $state<TagmaStatusSummary | undefined>(undefined);

  private backend: ManagementBackend | null = null;
  private rosterStop: (() => void) | null = null;
  private contextStop: (() => void) | null = null;
  private contexts = new Map<string, number>();
  // Online-only: the roster backstop interval armed alongside the
  // projection feed (a silently dead SSE looks alive, so the poll is the
  // only thing that notices). Null on the Offline path, which owns its
  // intervals outright.
  private rosterBackstopStop: (() => void) | null = null;
  private lastSubSignature = "";
  // Denominator data: the profile registry pulled once per attach (it
  // changes rarely and only takes effect on apply), and each agent's
  // active profile id captured by the slow context poll.
  private profileWindows = new Map<string, number>();
  private profileIds = new Map<string, string>();
  // Interval re-entry guards: both refreshes are async, and a slow network
  // must not stack a second concurrent round on a still-running one.
  private rosterRunning = false;
  private contextsRunning = false;
  // Gap-fill state: agents whose context pull failed cool down before a
  // roster-triggered retry (the 30s tick remains the final backstop),
  // so a persistently failing agent cannot recurse with the roster.
  private contextCooldown = new Map<string, number>();
  // Root-row signature: the root row keeps its object identity across
  // refreshes when nothing observable changed. Svelte skips the update
  // for a same-reference $state write, so effects that read rootRow do
  // not re-run -- a fresh reference per refresh would make every frame
  // re-run the chat page's attach effect, teardown the projection feed
  // and re-dial the SSE stream (the production refresh storm).
  private lastRootSignature = "";
  private gapPullRunning = false;
  /** Set once the first contexts round has been attempted (success or
   * not): the roster-triggered first pull must fire exactly once even
   * when every getAgentStatus fails, or the two recurse. */

  private contextsPrimed = false;
  /** Page unmount: stop the pollers, keep every cached row/number so a
   * return to the page paints instantly and only background-refreshes. */
  suspend(): void {
    this.rosterStop?.();
    this.rosterBackstopStop?.();
    this.contextStop?.();
    this.rosterStop = null;
    this.rosterBackstopStop = null;
    this.contextStop = null;
    this.backend = null;
  }

  /** Full reset (mode switch / logout): pollers and all cached data. */
  detach(): void {
    this.suspend();
    this.rootRow = null;
    this.subRows = [];
    this.summary = undefined;
    this.contexts.clear();
    this.profileWindows.clear();
    this.profileIds.clear();
    this.lastSubSignature = "";
    this.lastRootSignature = "";
    this.contextsPrimed = false;
    this.contextCooldown.clear();
  }

  attach(backend: ManagementBackend, backstopMs = BACKSTOP_INTERVAL_MS): void {
    // Idempotent re-attach: returning to the page keeps the warm cache
    // (rows, contexts, windows) and only background-refreshes, so the
    // panel paints instantly from the previous visit's data.
    // Reads the signature field, not the $state rootRow: a reactive read
    // here would make the chat page's attach effect depend on rootRow,
    // so every rootRow write would re-run it (full backend rebuild +
    // feed teardown per write). The signature field mirrors rootRow's
    // data without being reactive.
    const resuming = this.backend === null && this.lastRootSignature !== "";
    this.suspend();
    this.backend = backend;
    this.refreshRoster();
    if (backend.projectionFeed) {
      // The lesche's dirty SSE drives both refreshes the moment a
      // frame lands. A silently dead stream looks alive, so the visible
      // backstop intervals run alongside it (30s both, matching the
      // Offline cadence's slow leg): the poll is the only thing that
      // notices the feed died. The running guards collapse a dirty tick
      // and an interval tick that overlap, and the signature guards make
      // a redundant pull a no-op repaint.
      const feed = backend.projectionFeed;
      this.rosterStop = feed.subscribe(() => {
        void this.refreshRoster();
        void this.refreshContexts();
      });
      this.rosterBackstopStop = startVisibleInterval(
        () => this.refreshRoster(),
        backstopMs,
      );
      this.contextStop = startVisibleInterval(
        () => this.refreshContexts(),
        backstopMs,
      );
    } else {
      this.rosterStop = startVisibleInterval(
        () => this.refreshRoster(),
        15_000,
      );
      this.contextStop = startVisibleInterval(
        () => this.refreshContexts(),
        30_000,
      );
    }
    void this.refreshProfileWindows(backend);
    if (resuming) this.refreshContexts();
  }
  /** Event-driven roster refresh: the chat page routes every status-snapshot
   * update (the relay `tagma_status` push online, the direct SSE drain
   * offline) into this, so state flips paint at once instead of waiting for
   * the reconciliation poll. The re-entry guard collapses overlapping
   * nudges; the interval remains the dropped-frame backstop. */
  nudge(): void {
    void this.refreshRoster();
  }
  /** Mirror write for the aggregate snapshot. Both transports call this
   * at their dispatch boundary; `undefined` (offline eviction, no data
   * yet) flips the drawer summary to its waiting placeholder. */
  setSummary(s: TagmaStatusSummary | undefined): void {
    this.summary = s;
  }

  private async refreshRoster(): Promise<void> {
    if (this.rosterRunning) return;
    const backend = this.backend;
    if (!backend) return;
    this.rosterRunning = true;
    try {
      const resp = await backend.listAgents();
      if (this.backend !== backend) return; // detached mid-flight
      let root: StatusCardRow | undefined;
      const subs: StatusCardRow[] = [];
      for (const a of resp.agents) {
        const row: StatusCardRow = {
          id: a.id,
          state: a.state,
          role: a.role,
          activity: a.activity,
          description: a.description ?? "",
          contextTokens: this.contextOf(a),
          contextWindow: this.windowOf(a.id),
          parkedReason: a.parked_reason ?? null,
        };
        if (a.created_by === null) root = row;
        else subs.push(row);
      }
      // JSON-array encoding, not a ":" join: activity and description
      // are free text that could contain the separator and collide.
      const rootSignature =
        root === undefined
          ? ""
          : JSON.stringify([
              root.id,
              root.state,
              root.role,
              root.activity,
              root.contextTokens,
              root.description,
              root.contextWindow,
              root.parkedReason,
            ]);
      if (rootSignature !== this.lastRootSignature) {
        this.lastRootSignature = rootSignature;
        this.rootRow = root ?? null;
      }
      subs.sort(
        (a, b) =>
          STATE_ORDER[a.state] - STATE_ORDER[b.state] ||
          (a.id < b.id ? -1 : a.id > b.id ? 1 : 0),
      );
      // Keep the previous array (and row objects) when nothing observable
      // changed: Svelte skips the update, so DOM rows do not reshuffle.
      const signature = subs
        .map(
          (r) =>
            `${r.id}:${r.state}:${r.role}:${r.activity}:${r.contextTokens}:${r.description}:${r.contextWindow}`,
        )
        .join("|");
      if (signature !== this.lastSubSignature) {
        this.lastSubSignature = signature;
        this.subRows = subs;
      }
      // First roster with rows: pull contexts right away instead of
      // waiting for the first 30s tick (the numbers were invisible for
      // a full period otherwise).
      if ((root !== undefined || subs.length > 0) && !this.contextsPrimed) {
        void this.refreshContexts();
        this.contextsPrimed = true;
      }
      // Gap fill: agents that appeared after the first pull (or whose
      // pull failed and left cooldown) get an incremental retry here,
      // so a late joiner never waits a full 30s period for its number.
      void this.refreshMissingContexts();
    } catch {
      /* transient poll failure: keep the last roster */
    } finally {
      this.rosterRunning = false;
    }
  }

  /** Slow poll: context occupancy for live agents, one request each. */
  private async refreshContexts(): Promise<void> {
    if (this.contextsRunning) return;
    const backend = this.backend;
    if (!backend) return;
    this.contextsRunning = true;
    this.contextsPrimed = true;
    try {
      const targets = [
        ...(this.rootRow ? [this.rootRow] : []),
        ...this.subRows,
      ].filter((r) => r.state !== "faulted" && r.state !== "parked");
      // Parallel, not serial: N agents cost one RTT round, not N. The
      // per-agent catch keeps one dead agent from sinking the round.
      await Promise.all(
        targets.map(async (row) => {
          try {
            const status = await backend.getAgentStatus(row.id);
            if (this.backend !== backend) return;
            this.contexts.set(
              row.id,
              status.context.turn_tokens +
                status.context.pinned_items.reduce((sum, [, n]) => sum + n, 0),
            );
            const pid = status.profile?.profile_id;
            if (pid) this.profileIds.set(row.id, pid);
          } catch {
            // agent gone or not yet responsive: it cools down like any
            // other failure, so the gap fill does not hot-loop on it
            this.contextCooldown.set(row.id, Date.now());
          }
        }),
      );
      this.refreshRoster(); // re-merge context numbers into rows
    } finally {
      this.contextsRunning = false;
    }
  }

  /** Incremental gap fill, triggered from every roster merge: pull only
   * the live agents whose context is missing (late joiners, and agents
   * whose earlier pull failed). Failed ids cool down so a persistently
   * failing agent retries at most once per cooldown window instead of
   * recursing with the roster (the 30s poll stays the backstop). */
  private async refreshMissingContexts(): Promise<void> {
    if (this.gapPullRunning || !this.backend) return;
    const backend = this.backend;
    const now = Date.now();
    const targets = [
      ...(this.rootRow ? [this.rootRow] : []),
      ...this.subRows,
    ].filter(
      (r) =>
        r.state !== "faulted" &&
        r.state !== "parked" &&
        !this.contexts.has(r.id) &&
        (this.contextCooldown.get(r.id) ?? 0) <=
          now - CONTEXT_RETRY_COOLDOWN_MS,
    );
    if (targets.length === 0) return;
    this.gapPullRunning = true;
    try {
      await Promise.all(
        targets.map(async (row) => {
          try {
            const status = await backend.getAgentStatus(row.id);
            if (this.backend !== backend) return;
            this.contexts.set(
              row.id,
              status.context.turn_tokens +
                status.context.pinned_items.reduce((sum, [, n]) => sum + n, 0),
            );
            this.contextCooldown.delete(row.id);
            const pid = status.profile?.profile_id;
            if (pid) this.profileIds.set(row.id, pid);
          } catch {
            this.contextCooldown.set(row.id, Date.now());
          }
        }),
      );
      this.refreshRoster();
    } finally {
      this.gapPullRunning = false;
    }
  }
  private contextOf(a: { id: string; state: AgentState }): number | null {
    if (a.state === "faulted" || a.state === "parked") return null;
    return this.contexts.get(a.id) ?? null;
  }

  /** Registry pull for context denominators (fire-and-forget, best
   * effort: on failure every row shows "x / —" until the next attach). */
  private async refreshProfileWindows(
    backend: ManagementBackend,
  ): Promise<void> {
    try {
      const config = await backend.getProfiles();
      if (this.backend !== backend) return;
      for (const set of Object.values(config.sets)) {
        for (const p of set.profiles) {
          this.profileWindows.set(p.id, p.max_context_window);
        }
      }
      this.refreshRoster(); // re-merge windows into rows
    } catch {
      /* registry unavailable: denominators stay null */
    }
  }

  private windowOf(id: string): number | null {
    const pid = this.profileIds.get(id);
    return pid ? (this.profileWindows.get(pid) ?? null) : null;
  }
}

export const statusCardStore = new StatusCardStore();
