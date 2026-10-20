// The manage plane's `GET /agents` answer, as the tagma sends it: the roster
// plus the upstream fact roll-up beside it. The root is one entry of `agents`,
// never the body itself -- reading the body as an array finds nothing, which is
// exactly how a caller ends up resolving no root at all and reporting nothing.
import { manageOk, type RelayChannel } from "@kallipai/kallipai-keryx-client";

interface AgentRoster {
  agents?: { id?: unknown; created_by?: unknown }[];
}

/** What a channel said about its tagma's root agent. The cache hangs on the
 *  channel object: every caller on a live tunnel shares one answer, and a
 *  closed channel takes its entry with it, so a reconnect asks again instead of
 *  reusing an id that outlived the tunnel it came from. */
const roots = new WeakMap<RelayChannel, Promise<string | null>>();

/** The root agent id a tagma's `/agents/{id}/…` routes expect. The manage plane
 *  resolves no root alias, so the client reads the roster and picks the one
 *  agent `created_by` nothing (the registry's documented root invariant).
 *
 *  `null` when the roster cannot be read or carries no such agent. Negative
 *  answers are not cached: the next call asks again rather than pinning a
 *  transient failure for the life of the tunnel. */
export function relayRootAgentId(
  channel: RelayChannel,
): Promise<string | null> {
  const cached = roots.get(channel);
  if (cached !== undefined) return cached;
  const pending = resolveRootAgentId(channel).then((id) => {
    if (id === null) roots.delete(channel);
    return id;
  });
  roots.set(channel, pending);
  return pending;
}

async function resolveRootAgentId(
  channel: RelayChannel,
): Promise<string | null> {
  try {
    const res = await channel.manage("GET", "/agents");
    if (!manageOk(res)) return null;
    const agents = (res.body as AgentRoster | null)?.agents;
    if (!Array.isArray(agents)) return null;
    const root = agents.find((a) => a?.created_by === null);
    const id = root?.id;
    return typeof id === "string" && id.length > 0 ? id : null;
  } catch {
    return null;
  }
}
