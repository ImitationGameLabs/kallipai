// Agents store: agent roster + actions (interrupt, wake, duty toggle,
// remove, metadata update). No per-agent budget data (budget is tagma-wide).

import type { WireAgentManagementSummary } from "@kallipai/kallipai-client";
import { SvelteSet } from "svelte/reactivity";
import { type ManagementBackend, managementBackend } from "./client.ts";
import { startVisibleInterval } from "../visibleInterval.ts";
import { displayError } from "./errors.ts";
import {
  manage_agents_action_failed,
  manage_agents_load_failed,
} from "../../paraglide/messages.js";

class AgentsStore {
  private _backend: ManagementBackend | null = null;

  private get backend(): ManagementBackend {
    if (this._backend === null) this._backend = managementBackend();
    return this._backend;
  }
  agents = $state<WireAgentManagementSummary[]>([]);
  isLoading = $state(false);
  hasLoaded = $state(false);
  error = $state<string | null>(null);

  private pollStop: (() => void) | null = null;
  private inFlight = new SvelteSet<string>();
  private snapshots = new Map<string, WireAgentManagementSummary>();

  get idleCount(): number {
    return this.agents.filter((a) => a.state === "idle").length;
  }
  get busyCount(): number {
    return this.agents.filter((a) => a.state === "busy").length;
  }
  get faultedCount(): number {
    return this.agents.filter((a) => a.state === "faulted").length;
  }

  async refresh(force = false): Promise<void> {
    if (!force && this.inFlight.size > 0) return; // suppress poll during in-flight mutation
    this.isLoading = true;
    this.error = null;
    try {
      const resp = await this.backend.listAgents();
      this.agents = [...resp.agents];
      this.hasLoaded = true;
    } catch (e) {
      this.error = displayError("agents", e, manage_agents_load_failed());
    } finally {
      this.isLoading = false;
    }
  }

  /** With a live projection feed the dirty SSE drives refreshes
   * and the interval is retired; without one (Offline) the visible-
   * paused interval remains the reconciliation backstop. Management
   * actions still refresh optimistically either way. */
  startPolling(intervalMs = 30_000): void {
    this.stopPolling();
    const feed = this.backend.projectionFeed;
    if (feed) {
      this.pollStop = feed.subscribe(() => void this.refresh());
    } else {
      this.pollStop = startVisibleInterval(() => this.refresh(), intervalMs);
    }
    this.refresh();
  }

  stopPolling(): void {
    this.pollStop?.();
    this.pollStop = null;
  }

  /** Switch backend. Resets all state. */
  switchBackend(backend: ManagementBackend): void {
    this.stopPolling();
    this._backend = backend;
    this.isLoading = false;
    this.agents = [];
    this.error = null;
    this.hasLoaded = false;
    this.snapshots.clear();
    this.inFlight.clear();
  }

  async interrupt(id: string): Promise<void> {
    this.inFlight.add(id);
    // Optimistic: flip busy → idle.
    this.optimisticUpdate(id, (a) =>
      a.state === "busy" ? { ...a, state: "idle" as const, activity: "" } : a,
    );
    try {
      await this.backend.interruptAgent(id);
      this.snapshots.delete(id);
    } catch (e) {
      this.revertById(id);
      this.error = displayError("agents", e, manage_agents_action_failed());
      throw e;
    } finally {
      this.inFlight.delete(id);
    }
  }

  async toggleDuty(id: string): Promise<void> {
    const agent = this.agents.find((a) => a.id === id);
    if (!agent) return;
    const nextDuty = agent.duty === "onduty" ? "offduty" : "onduty";
    this.optimisticUpdate(id, (a) => ({ ...a, duty: nextDuty }));
    this.inFlight.add(id);
    try {
      await this.backend.setAgentDuty(id, { status: nextDuty });
      this.snapshots.delete(id);
    } catch (e) {
      this.revertById(id);
      this.error = displayError("agents", e, manage_agents_action_failed());
      throw e;
    } finally {
      this.inFlight.delete(id);
    }
  }

  async remove(id: string): Promise<void> {
    this.inFlight.add(id);
    const snapshot = this.agents;
    this.agents = this.agents.filter((a) => a.id !== id);
    try {
      await this.backend.removeAgent(id);
    } catch (e) {
      this.agents = snapshot; // revert
      this.error = displayError("agents", e, manage_agents_action_failed());
      throw e;
    } finally {
      this.inFlight.delete(id);
    }
  }

  async updateMetadata(
    id: string,
    body: { role?: string; description?: string },
  ): Promise<void> {
    this.inFlight.add(id);
    this.optimisticUpdate(id, (a) => ({ ...a, ...body }));
    try {
      await this.backend.updateAgentMetadata(id, body);
      this.snapshots.delete(id);
    } catch (e) {
      this.revertById(id);
      this.error = displayError("agents", e, manage_agents_action_failed());
      throw e;
    } finally {
      this.inFlight.delete(id);
    }
  }

  isInFlight(id: string): boolean {
    return this.inFlight.has(id);
  }

  // --- internals ---

  private optimisticUpdate(
    id: string,
    fn: (a: WireAgentManagementSummary) => WireAgentManagementSummary,
  ): void {
    const idx = this.agents.findIndex((a) => a.id === id);
    if (idx === -1) return;
    const current = this.agents[idx]!;
    this.snapshots.set(id, current);
    const updated = fn(current);
    this.agents = this.agents.map((a, i) => (i === idx ? updated : a));
  }

  private revertById(id: string): void {
    const snap = this.snapshots.get(id);
    if (!snap) return;
    this.agents = this.agents.map((a) => (a.id === id ? snap : a));
    this.snapshots.delete(id);
  }
}

export const agentsStore = new AgentsStore();
