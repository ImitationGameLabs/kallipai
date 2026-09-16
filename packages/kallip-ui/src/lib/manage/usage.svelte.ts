// Usage store: tagma-wide single-launch token usage totals.
//
// The totals ride on every agent status response — the backend computes the
// sums over all recorded agents — so one status request per poll cycle
// suffices; the request count never scales with the agent count. The
// roster's first non-faulted agent serves as the request carrier only
// (faulted agents reject the status endpoint); its identity is
// getter from the page (this store stays decoupled from the agents store,
// like every other manage store). A failed refresh keeps the last known
// totals: the card is informational and the next poll retries.

import type { TagmaUsageTotals } from "@kallipai/kallip-client";
import { type ManagementBackend, managementBackend } from "./client.ts";
import { startVisibleInterval } from "../visibleInterval.ts";

class UsageStore {
  private _backend: ManagementBackend | null = null;
  private carrier: (() => string | undefined) | null = null;
  private pollStop: (() => void) | null = null;

  private get backend(): ManagementBackend {
    if (this._backend === null) this._backend = managementBackend();
    return this._backend;
  }

  totals = $state<TagmaUsageTotals | null>(null);

  async refresh(): Promise<void> {
    const id = this.carrier?.();
    if (!id) {
      this.totals = null;
      return;
    }
    try {
      const resp = await this.backend.getAgentStatus(id);
      this.totals = resp.usage_totals ?? null;
    } catch {
      /* keep the last known totals; the next poll retries */
    }
  }

  /** Reconciliation backstop for a mounted page, not a live feed — same
   * shape as the budget store (visible-paused interval). */
  startPolling(carrier: () => string | undefined, intervalMs = 30_000): void {
    this.stopPolling();
    this.carrier = carrier;
    this.pollStop = startVisibleInterval(() => this.refresh(), intervalMs);
    this.refresh();
  }

  stopPolling(): void {
    this.pollStop?.();
    this.pollStop = null;
  }

  /** Switch backend (offline→online or vice versa). Resets all state. */
  switchBackend(backend: ManagementBackend): void {
    this.stopPolling();
    this._backend = backend;
    this.totals = null;
  }
}

export const usageStore = new UsageStore();
