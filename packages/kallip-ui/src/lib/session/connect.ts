import { DaemonClient, DaemonSession } from "@kallipai/kallip-client";
import type { AgentId } from "@kallipai/kallip-common";
import type { OfflineModeConfig } from "../config/config.ts";

/**
 * Connect to the daemon and bind to its single root agent (eagerly created at
 * daemon startup). Mirrors kallip-tui's `Session::connect`.
 */
export async function connectDirect(
  config: OfflineModeConfig,
): Promise<DaemonSession> {
  const client = new DaemonClient({
    baseUrl: config.daemonUrl,
    authToken: config.authToken,
  });

  const root = await client.getRootAgent();
  const agentId: AgentId = root.id;

  return new DaemonSession(client, agentId);
}
