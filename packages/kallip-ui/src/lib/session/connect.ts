import { TagmaClient } from "@kallipai/kallip-client";
import type { AgentId } from "@kallipai/kallip-common";
import type { OfflineModeConfig } from "../config/config.ts";
import type { ConversationSender } from "../transcript.ts";
import { DirectTransport } from "./directTransport.ts";

/** The result of connecting to the tagma directly: the transport bound to the
 * root agent, plus the tagma's conversation id (when enrolled) the offline path
 * shares with the online path for its IndexedDB cache + history pulls. The
 * conversation id is `null` for a never-enrolled (pure-offline) tagma — there is
 * no durable history, and the local conversation is keyed `"local"`. */
export interface DirectConnection {
  readonly transport: DirectTransport;
  readonly conversationId: string | null;
}

/**
 * Connect to the tagma and bind a {@link DirectTransport} to its single root
 * agent (eagerly created at tagma startup). The transport consumes the tagma's
 * external chat-room API (`/agents/{id}/external/events` + the inbound message
 * POST). Mirrors kallip-tui's `Session::connect`. Also surfaces the tagma's
 * conversation id so offline + online share one cache, and the offline local
 * user identity (so the optimistic bubble renders the operator's handle).
 */
export async function connectDirect(
  config: OfflineModeConfig,
): Promise<DirectConnection> {
  const client = new TagmaClient({
    baseUrl: config.tagmaUrl,
    authToken: config.authToken,
  });

  const root = await client.getRootAgent();
  const agentId: AgentId = root.id;
  const localSender: ConversationSender = {
    kind: "user",
    id: root.local_user_id ?? "local-operator",
    handle: root.local_user_handle ?? "Operator",
  };

  return {
    transport: new DirectTransport(client, agentId, localSender),
    conversationId: root.conversation_id ?? null,
  };
}
