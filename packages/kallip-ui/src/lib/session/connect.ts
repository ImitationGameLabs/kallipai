import { TagmaClient, TagmaSession } from "@kallipai/kallip-client";
import type { AgentId } from "@kallipai/kallip-common";
import type { OfflineModeConfig } from "../config/config.ts";

/**
 * Connect to the tagma and bind to its single root agent (eagerly created at
 * tagma startup). Mirrors kallip-tui's `Session::connect`.
 */
export async function connectDirect(
  config: OfflineModeConfig,
): Promise<TagmaSession> {
  const client = new TagmaClient({
    baseUrl: config.tagmaUrl,
    authToken: config.authToken,
  });

  const root = await client.getRootAgent();
  const agentId: AgentId = root.id;

  return new TagmaSession(client, agentId);
}
