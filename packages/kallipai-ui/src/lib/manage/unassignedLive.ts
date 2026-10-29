// Unassigned-live advisory refresh for the profiles page.
//
// unassignedLiveSnapshot (profiles-view) owns the pure intersection; this module
// owns the orchestration around it: the roster fetch and the per-agent
// allSettled fan-out. Refreshes are event-driven (a card entering or
// leaving the unassigned area, and apply), never polled. An always-on
// variant needs the list endpoint to carry the active profile id; that is
// a backend change, not a poll.

import type { AgentStatusResponse } from "@kallipai/kallipai-client";
import { unassignedLiveSnapshot } from "./profiles-view.ts";

/** Narrow backend seam: tests stub two methods instead of the whole backend. */
export interface UnassignedLiveBackend {
  listAgents(): Promise<{ agents: readonly { id: string }[] }>;
  getAgentStatus(id: string): Promise<AgentStatusResponse>;
}

export interface UnassignedLiveSnapshot {
  agentCount: number;
  profileIds: string[];
}

/** `refreshed: false` = roster failure; the caller keeps the previous
 * snapshot (advisory only). `snapshot: null` = nothing unassigned-live. */
export type UnassignedLiveResult =
  | { refreshed: true; snapshot: UnassignedLiveSnapshot | null }
  | { refreshed: false };

export async function refreshUnassignedLive(
  backend: UnassignedLiveBackend,
  unassigned: readonly { id: string }[],
): Promise<UnassignedLiveResult> {
  if (unassigned.length === 0) {
    return { refreshed: true, snapshot: null };
  }
  const unassignedIds = unassigned.map((p) => p.id);
  try {
    const { agents } = await backend.listAgents();
    // Per-agent catch (allSettled): one 409/404 must not void the whole
    // snapshot — the advisory layer reports what it could see.
    const statuses = await Promise.allSettled(
      agents.map((a) => backend.getAgentStatus(a.id)),
    );
    return {
      refreshed: true,
      snapshot: unassignedLiveSnapshot(unassignedIds, statuses),
    };
  } catch {
    return { refreshed: false };
  }
}
