import { assertEquals } from "@std/assert";
import type { AgentStatusResponse } from "@kallipai/kallipai-client";
import { type ParkedLiveBackend, refreshParkedLive } from "./parkedLive.ts";

/** Stub backend: the roster is the map's keys; each agent runs the mapped
 * profile id, or rejects when the value is an Error. */
function stubBackend(
  statuses: Record<string, string | Error>,
): ParkedLiveBackend {
  return {
    listAgents: () =>
      Promise.resolve({ agents: Object.keys(statuses).map((id) => ({ id })) }),
    getAgentStatus: (id: string) => {
      const v = statuses[id]!;
      return v instanceof Error
        ? Promise.reject(v)
        : Promise.resolve({
            profile: { profile_id: v },
          } as AgentStatusResponse);
    },
  };
}

Deno.test(
  "refreshParkedLive: empty parking clears without a roster call",
  async () => {
    let calls = 0;
    const backend: ParkedLiveBackend = {
      listAgents: () => {
        calls++;
        return Promise.resolve({ agents: [] });
      },
      getAgentStatus: () => Promise.resolve({} as AgentStatusResponse),
    };
    assertEquals(await refreshParkedLive(backend, []), {
      refreshed: true,
      snapshot: null,
    });
    assertEquals(calls, 0);
  },
);

Deno.test(
  "refreshParkedLive: snapshot counts live agents at the parked intersection",
  async () => {
    const backend = stubBackend({ a1: "p1", a2: "p1", a3: "p2", a4: "other" });
    assertEquals(
      await refreshParkedLive(backend, [{ id: "p1" }, { id: "p2" }]),
      {
        refreshed: true,
        snapshot: { agentCount: 3, profileIds: ["p1", "p2"] },
      },
    );
  },
);

Deno.test(
  "refreshParkedLive: one rejected status is skipped, not fatal",
  async () => {
    const backend = stubBackend({ a1: "p1", a2: new Error("409") });
    assertEquals(await refreshParkedLive(backend, [{ id: "p1" }]), {
      refreshed: true,
      snapshot: { agentCount: 1, profileIds: ["p1"] },
    });
  },
);

Deno.test(
  "refreshParkedLive: roster failure reports keep-previous",
  async () => {
    let fail = false;
    const ok = stubBackend({ a1: "p1" });
    const backend: ParkedLiveBackend = {
      listAgents: () =>
        fail ? Promise.reject(new Error("roster down")) : ok.listAgents(),
      getAgentStatus: ok.getAgentStatus,
    };
    assertEquals(await refreshParkedLive(backend, [{ id: "p1" }]), {
      refreshed: true,
      snapshot: { agentCount: 1, profileIds: ["p1"] },
    });
    fail = true;
    assertEquals(await refreshParkedLive(backend, [{ id: "p1" }]), {
      refreshed: false,
    });
  },
);
