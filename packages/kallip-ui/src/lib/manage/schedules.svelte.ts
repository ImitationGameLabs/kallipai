// Schedules store: the tagma's single work schedule (GET/PUT /work-schedule).

import type {
  PutWorkScheduleRequest,
  WorkSchedule,
} from "@kallipai/kallip-client";
import { type ManagementBackend, managementBackend } from "./client.ts";

class SchedulesStore {
  schedule = $state<WorkSchedule | null>(null);
  isLoading = $state(false);
  hasLoaded = $state(false);
  isSaving = $state(false);
  error = $state<string | null>(null);
  private _backend: ManagementBackend | null = null;

  private get backend(): ManagementBackend {
    if (this._backend === null) this._backend = managementBackend();
    return this._backend;
  }

  /** Switch backend. Resets all state. */
  switchBackend(backend: ManagementBackend): void {
    this.schedule = null;
    this.error = null;
    this.hasLoaded = false;
    this.isLoading = false;
    this.isSaving = false;
    this._backend = backend;
  }

  async refresh(force = false): Promise<void> {
    // Suppress the poll while a mutation is in flight: its response is
    // the newer truth and would flash a stale row mid-save.
    if (!force && (this.isLoading || this.isSaving)) return;
    this.isLoading = true;
    this.error = null;
    try {
      this.schedule = await this.backend.getWorkSchedule();
      this.hasLoaded = true;
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
    } finally {
      this.isLoading = false;
    }
  }

  /** PUT the whole schedule; the response is the new server-side truth. */
  async save(body: PutWorkScheduleRequest): Promise<WorkSchedule> {
    this.isSaving = true;
    this.error = null;
    try {
      const saved = await this.backend.putWorkSchedule(body);
      this.schedule = saved;
      return saved;
    } catch (e) {
      this.error = e instanceof Error ? e.message : String(e);
      throw e;
    } finally {
      this.isSaving = false;
    }
  }
}

export const schedulesStore = new SchedulesStore();
