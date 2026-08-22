// Budget store: tagma-wide token budget status + mutations.
//
// Polls GET /budget on an interval while mounted. Mutations (adjust, set
// remaining, pause all) use optimistic updates with revert-on-error.
// An inFlightMutation flag suppresses auto-refresh polls so the optimistic
// state isn't overwritten by a stale server response.

import type { BudgetResponse } from "@kallipai/kallip-client";
import {
  isBudgetPaused as computeIsPaused,
  consumedPct as computeConsumedPct,
  burnRate as computeBurnRate,
  etaMinutes as computeEta,
  type BudgetSample,
} from "./compute.ts";
import { type ManagementBackend, managementBackend } from "./client.ts";

class BudgetStore {
  private _backend: ManagementBackend | null = null;

  private get backend(): ManagementBackend {
    if (this._backend === null) this._backend = managementBackend();
    return this._backend;
  }
  budget = $state(0);
  consumed = $state(0);
  remaining = $state(0);
  isLoading = $state(false);
  error = $state<string | null>(null);

  /** Tagma-wide budget is paused (remaining === 0). */
  get isPaused(): boolean {
    return computeIsPaused(this.remaining, this.budget);
  }

  /** Percentage of budget consumed (0–100). */
  get consumedPct(): number {
    return computeConsumedPct(this.consumed, this.budget);
  }

  private samples: BudgetSample[] = [];
  private inFlightMutation = $state(false);

  /** True while a budget mutation is in flight (adjust, set, pause). */
  get isBusy(): boolean {
    return this.inFlightMutation;
  }
  private pollHandle: ReturnType<typeof setInterval> | null = null;

  /** Burn rate (tokens/min) computed from recent samples. Null if insufficient data. */
  get burnRate(): number | null {
    return computeBurnRate(this.samples);
  }

  /** Estimated minutes until budget exhaustion at current burn rate. Null if idle. */
  get etaMinutes(): number | null {
    return computeEta(this.remaining, this.burnRate);
  }

  async refresh(): Promise<void> {
    if (this.inFlightMutation) return;
    this.isLoading = true;
    this.error = null;
    try {
      const resp = await this.backend.getBudget();
      this.applyResponse(resp);
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    } finally {
      this.isLoading = false;
    }
  }

  startPolling(intervalMs = 5000): void {
    this.stopPolling();
    this.pollHandle = setInterval(() => this.refresh(), intervalMs);
    this.refresh();
  }

  stopPolling(): void {
    if (this.pollHandle !== null) {
      clearInterval(this.pollHandle);
      this.pollHandle = null;
    }
    this.samples = [];
  }

  /** Switch backend (offline→online or vice versa). Resets all state. */
  switchBackend(backend: ManagementBackend): void {
    this.stopPolling();
    this._backend = backend;
    this.isLoading = false;
    this.budget = 0;
    this.consumed = 0;
    this.remaining = 0;
    this.error = null;
    this.inFlightMutation = false;
    this.samples = [];
  }

  /** Adjust budget by delta. Optimistic. Returns the server response or throws. */
  async adjust(delta: number): Promise<BudgetResponse> {
    if (delta === 0) throw new Error("delta must be non-zero");
    return this.mutate(async () => {
      const resp = await this.backend.updateBudget({ delta });
      return resp;
    }, delta);
  }

  /** Set remaining budget to an exact value. Optimistic. */
  async setRemaining(value: number): Promise<BudgetResponse> {
    return this.mutate(async () => {
      return await this.backend.updateBudget({ set_remaining: value });
    }, value - this.remaining);
  }

  /** Pause all agents by setting remaining to 0. */
  async pauseAll(): Promise<BudgetResponse> {
    return this.mutate(async () => {
      return await this.backend.updateBudget({ set_remaining: 0 });
    }, -this.remaining);
  }

  // --- internals ---

  private applyResponse(resp: BudgetResponse): void {
    this.budget = resp.budget;
    this.consumed = resp.consumed;
    this.remaining = resp.remaining;
    // Track samples for burn-rate computation (keep last 6).
    this.samples.push({ consumed: resp.consumed, timestamp: Date.now() });
    if (this.samples.length > 6) this.samples.shift();
  }

  private async mutate(
    fn: () => Promise<BudgetResponse>,
    optimisticDelta: number,
  ): Promise<BudgetResponse> {
    // Snapshot for revert.
    const prev = {
      budget: this.budget,
      consumed: this.consumed,
      remaining: this.remaining,
    };
    // Apply optimistic.
    this.inFlightMutation = true;
    if (optimisticDelta !== 0) {
      this.remaining = Math.max(0, this.remaining + optimisticDelta);
      this.budget = this.consumed + this.remaining;
    }
    try {
      const resp = await fn();
      this.applyResponse(resp);
      return resp;
    } catch (e) {
      // Revert on error.
      this.budget = prev.budget;
      this.consumed = prev.consumed;
      this.remaining = prev.remaining;
      this.error = e instanceof Error ? e.message : String(e);
      throw e;
    } finally {
      this.inFlightMutation = false;
    }
  }
}

export const budgetStore = new BudgetStore();
