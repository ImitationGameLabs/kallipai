import { DaemonClient, DaemonSession } from "@kallipai/kallip-client";
import type { AgentId } from "@kallipai/kallip-common";
import type { OfflineModeConfig } from "../config/config.ts";

/**
 * Connect to a daemon, reusing an existing root agent (created_by == null) or
 * spawning one labelled role="root". Returns a DaemonSession bound to that
 * agent. Mirrors kallip-tui's Session::connect.
 */
export async function connectDirect(
  config: OfflineModeConfig,
): Promise<DaemonSession> {
  const client = new DaemonClient({
    baseUrl: config.daemonUrl,
    authToken: config.authToken,
  });

  const agents = await client.listAgents();
  const root = agents.find((a) => a.created_by == null);
  const agentId: AgentId = root
    ? root.id
    : await client.spawn({ role: "root", description: "Top-level agent" });

  return new DaemonSession(client, agentId);
}
