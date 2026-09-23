import { TagmaClient } from "@kallipai/kallip-client";
import type { AgentId } from "@kallipai/kallip-common";
import type { OfflineModeConfig } from "../config/config.ts";
import { LOCAL_OPERATOR_SENDER } from "../transcript.ts";
import { DirectTransport } from "./directTransport.ts";
import { loadTimezoneSetting } from "../time/stamp.svelte.ts";

/** The result of connecting to the tagma directly: the transport bound to the
 * root agent, plus the tagma's conversation id (when enrolled) the offline path
 * shares with the online path for its IndexedDB cache + history pulls. The
 * conversation id is `null` for a never-enrolled tagma; rows still persist to
 * the operator (`NULL`) partition, but with no tagma conversation-id the cache
 * falls back to the `"local"` key. */
export interface DirectConnection {
  readonly transport: DirectTransport;
  readonly conversationId: string | null;
}

/**
 * Connect to the tagma and bind a {@link DirectTransport} to its single root
 * agent (eagerly created at tagma startup). The transport consumes the tagma's
 * external chat-room API (`/agents/{id}/external/events` + the inbound message
 * POST). Eagerly connects and subscribes in one step. Also surfaces the tagma's
 * conversation id so offline + online share one cache.
 */
export async function connectDirect(
  config: OfflineModeConfig,
): Promise<DirectConnection> {
  const client = new TagmaClient({
    baseUrl: config.tagmaUrl,
    authToken: config.authToken,
  });

  const root = await client.getRootAgent();
  // Session-level timezone load: one GET per established session (the
  // single-flight guard in the stamp module dedupes concurrent callers),
  // covering every render site without each page re-fetching.
  void loadTimezoneSetting(() => client.getTimezone());
  const agentId: AgentId = root.id;

  return {
    transport: new DirectTransport(client, agentId, LOCAL_OPERATOR_SENDER),
    conversationId: root.conversation_id ?? null,
  };
}
