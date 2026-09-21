// Tasks store: the read-only task ledger (lightweight list rows + the
// selected task's detail). Mirrors the agents store's transport wiring:
// backend switching per channel state, live projection dirty feed when
// present, visible-interval polling as the offline backstop. The list
// polls rows only; the detail pane fetches once per selection and again
// when the selected row's status moves under it.

import type { TaskExport, TaskRow } from "@kallipai/kallip-client";
import { type ManagementBackend, managementBackend } from "./client.ts";
import { startVisibleInterval } from "../visibleInterval.ts";
import { clampPage } from "./compute.ts";
import { displayError } from "./errors.ts";
import { manage_tasks_load_failed } from "../../paraglide/messages.js";

/** Server-side page size for the row list. */
export const TASKS_PAGE_SIZE = 25;

class TasksStore {
  private _backend: ManagementBackend | null = null;

  private get backend(): ManagementBackend {
    if (this._backend === null) this._backend = managementBackend();
    return this._backend;
  }

  rows = $state<TaskRow[]>([]);
  total = $state(0);
  page = $state(0);
  selectedId = $state<number | null>(null);
  detail = $state<TaskExport | null>(null);
  showArchived = $state(false);
  isLoading = $state(false);
  hasLoaded = $state(false);
  error = $state<string | null>(null);

  private pollStop: (() => void) | null = null;

  async refresh(force = false): Promise<void> {
    if (!force && this.isLoading) return;
    this.isLoading = true;
    this.error = null;
    try {
      const result = await this.backend.listTasks({
        archived: this.showArchived,
        limit: TASKS_PAGE_SIZE,
        offset: this.page * TASKS_PAGE_SIZE,
      });
      const prev = this.rows.find((r) => r.id === this.selectedId);
      // Re-clamp the page against the fresh total: a shrinking ledger
      // can strand the current page past the new end.
      const clamped = clampPage(this.page, result.total, TASKS_PAGE_SIZE);
      const pageMoved = clamped !== this.page;
      this.page = clamped;
      this.rows = [...result.rows];
      this.total = result.total;
      if (pageMoved) {
        // The rows just read belong to the stranded page; refetch the
        // clamped page (forced, bounded: the clamped page is in range).
        void this.refresh(true);
        return;
      }
      // Keep the selection stable; fall back to the first row when it
      // disappeared from the current partition (e.g. the archive toggle).
      const ids = new Set(this.rows.map((t) => t.id));
      if (this.selectedId === null || !ids.has(this.selectedId)) {
        this.selectedId = this.rows[0]?.id ?? null;
        if (this.selectedId !== null) void this.loadDetail(this.selectedId);
      } else if (prev && this.selectedId !== null) {
        // The selected row updated: if its updated_at moved (status,
        // confirmations, notes), refresh the detail pane once so it stays
        // consistent with the list.
        const now = this.rows.find((r) => r.id === this.selectedId);
        if (now && now.updated_at !== prev.updated_at) {
          void this.loadDetail(this.selectedId);
        }
      }
      this.hasLoaded = true;
    } catch (e) {
      this.error = displayError("tasks", e, manage_tasks_load_failed());
    } finally {
      this.isLoading = false;
    }
  }

  select(id: number): void {
    this.selectedId = id;
    void this.loadDetail(id);
  }

  private async loadDetail(id: number): Promise<void> {
    try {
      const detail = await this.backend.getTask(id);
      // A slower earlier request must not overwrite the newer selection.
      if (this.selectedId !== id) {
        return;
      }
      this.detail = detail;
    } catch (e) {
      if (this.selectedId !== id) {
        return;
      }
      this.error = displayError("task detail", e, manage_tasks_load_failed());
    }
  }

  /** Flip the archive partition (mirrors the CLI's --archived flag). */
  toggleArchived(): void {
    this.showArchived = !this.showArchived;
    this.page = 0;
    void this.refresh(true);
  }

  /** Jump to a page (zero-based), clamped into the valid range. */
  goToPage(p: number): void {
    const clamped = clampPage(p, this.total, TASKS_PAGE_SIZE);
    if (clamped === this.page) return;
    this.page = clamped;
    void this.refresh(true);
  }

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
    this.rows = [];
    this.total = 0;
    this.page = 0;
    this.selectedId = null;
    this.detail = null;
    this.showArchived = false;
    this.error = null;
    this.hasLoaded = false;
  }
}

export const tasksStore = new TasksStore();
