// Parked-live advisory refresh for the profiles page.
//
// parkedLiveSnapshot (profiles-view) owns the pure intersection; this module
// owns the orchestration around it: the roster fetch and the per-agent
// allSettled fan-out. Refreshes are event-driven (parking/unparking
// mutations and apply), never polled — a later always-on variant needs the
// list endpoint to carry the active profile id; that is a backend change,
// not a poll.

import type { AgentStatusResponse } from "@kallipai/kallipai-client";
import { parkedLiveSnapshot } from "./profiles-view.ts";

/** Narrow backend seam: tests stub two methods instead of the whole backend. */
export interface ParkedLiveBackend {
  listAgents(): Promise<{ agents: readonly { id: string }[] }>;
  getAgentStatus(id: string): Promise<AgentStatusResponse>;
}

export interface ParkedLiveSnapshot {
  agentCount: number;
  profileIds: string[];
}

/** `refreshed: false` = roster failure; the caller keeps the previous
 * snapshot (advisory only). `snapshot: null` = nothing parked-live. */
export type ParkedLiveResult =
  | { refreshed: true; snapshot: ParkedLiveSnapshot | null }
  | { refreshed: false };

export async function refreshParkedLive(
  backend: ParkedLiveBackend,
  parked: readonly { id: string }[],
): Promise<ParkedLiveResult> {
  if (parked.length === 0) {
    return { refreshed: true, snapshot: null };
  }
  const parkedIds = parked.map((p) => p.id);
  try {
    const { agents } = await backend.listAgents();
    // Per-agent catch (allSettled): one 409/404 must not void the whole
    // snapshot — the advisory layer reports what it could see.
    const statuses = await Promise.allSettled(
      agents.map((a) => backend.getAgentStatus(a.id)),
    );
    return {
      refreshed: true,
      snapshot: parkedLiveSnapshot(parkedIds, statuses),
    };
  } catch {
    return { refreshed: false };
  }
}
