// Schedules store: work schedule CRUD with active/paused toggle.

import type {
  CreateWorkScheduleRequest,
  UpdateWorkScheduleRequest,
  WorkSchedule,
} from "@kallipai/kallip-client";
import { type ManagementBackend, managementBackend } from "./client.ts";
import { SvelteSet } from "svelte/reactivity";

class SchedulesStore {
  schedules = $state<WorkSchedule[]>([]);
  isLoading = $state(false);
  hasLoaded = $state(false);
  error = $state<string | null>(null);
  private inFlight = new SvelteSet<string>();
  private _backend: ManagementBackend | null = null;

  private get backend(): ManagementBackend {
    if (this._backend === null) this._backend = managementBackend();
    return this._backend;
  }

  /** Switch backend. Resets all state. */
  switchBackend(backend: ManagementBackend): void {
    this.schedules = [];
    this.error = null;
    this.hasLoaded = false;
    this.inFlight.clear();
    this.isLoading = false;
    this._backend = backend;
  }

  isInFlight(id: string): boolean {
    return this.inFlight.has(id);
  }

  get isCreating(): boolean {
    return this.inFlight.has("create");
  }

  get activeCount(): number {
    return this.schedules.filter((s) => s.status === "active").length;
  }
  get pausedCount(): number {
    return this.schedules.filter((s) => s.status === "paused").length;
  }

  async refresh(force = false): Promise<void> {
    if (!force && this.inFlight.size > 0) return; // suppress poll during in-flight mutation
    this.isLoading = true;
    this.error = null;
    try {
      const resp = await this.backend.listWorkSchedules();
      this.schedules = [...resp];
      this.hasLoaded = true;
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    } finally {
      this.isLoading = false;
    }
  }

  async create(body: CreateWorkScheduleRequest): Promise<WorkSchedule> {
    this.inFlight.add("create");
    this.error = null;
    try {
      const created = await this.backend.createWorkSchedule(body);
      this.schedules = [...this.schedules, created];
      return created;
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
      throw e;
    } finally {
      this.inFlight.delete("create");
    }
  }

  async update(id: string, body: UpdateWorkScheduleRequest): Promise<void> {
    // Optimistic: apply the change locally.
    const snapshot = this.schedules;
    this.inFlight.add(id);
    if (body.status) {
      this.schedules = this.schedules.map((s) =>
        s.id === id ? { ...s, status: body.status! } : s,
      );
    }
    try {
      const updated = await this.backend.updateWorkSchedule(id, body);
      this.schedules = this.schedules.map((s) =>
        s.id === id ? updated : s,
      );
    } catch (e) {
      this.schedules = snapshot;
      this.error = e instanceof Error ? e.message : String(e);
      throw e;
    } finally {
      this.inFlight.delete(id);
    }
  }

  async toggleStatus(id: string): Promise<void> {
    const schedule = this.schedules.find((s) => s.id === id);
    if (!schedule) return;
    const next = schedule.status === "active" ? "paused" : "active";
    await this.update(id, { status: next });
  }

  async remove(id: string): Promise<void> {
    const snapshot = this.schedules;
    this.inFlight.add(id);
    this.schedules = this.schedules.filter((s) => s.id !== id);
    try {
      await this.backend.deleteWorkSchedule(id);
    } catch (e) {
      this.schedules = snapshot;
      this.error = e instanceof Error ? e.message : String(e);
      throw e;
    } finally {
      this.inFlight.delete(id);
    }
  }
}

export const schedulesStore = new SchedulesStore();
